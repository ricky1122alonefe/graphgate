mod coordinator;
mod introspection;
mod response;

use std::collections::BTreeMap;

use futures_util::future::BoxFuture;
use spin::Mutex;
use tracing::instrument;
use value::{ConstValue, Name, Variables};

pub use coordinator::Coordinator;
pub use response::{ErrorPath, Response, ServerError};

use crate::planner::{
    FetchNode, FlattenNode, IntrospectionNode, ParallelNode, PathSegment, PlanNode, SequenceNode,
};
use crate::ComposedSchema;
use introspection::{IntrospectionRoot, Resolver};

pub struct Executor<'e, T> {
    schema: &'e ComposedSchema,
    resp: Mutex<Response>,
    coordinator: T,
}

impl<'e, T: Coordinator> Executor<'e, T> {
    pub fn new(schema: &'e ComposedSchema, coordinator: T) -> Self {
        Executor {
            schema,
            resp: Mutex::new(Response {
                data: ConstValue::Null,
                errors: Vec::new(),
            }),
            coordinator,
        }
    }

    pub async fn execute(self, node: &PlanNode<'_>) -> Response {
        self.execute_node(node).await;
        self.resp.into_inner()
    }

    fn execute_node<'a>(&'a self, node: &'a PlanNode<'_>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match node {
                PlanNode::Sequence(sequence) => self.execute_sequence_node(sequence).await,
                PlanNode::Parallel(parallel) => self.execute_parallel_node(parallel).await,
                PlanNode::Introspection(introspection) => {
                    self.execute_introspection_node(introspection)
                }
                PlanNode::Fetch(fetch) => self.execute_fetch_node(fetch).await,
                PlanNode::Flatten(flatten) => self.execute_flatten_node(flatten).await,
            }
        })
    }

    #[instrument(skip(self), level = "debug")]
    async fn execute_sequence_node(&self, sequence: &SequenceNode<'_>) {
        for node in &sequence.nodes {
            self.execute_node(node).await;
        }
    }

    #[instrument(skip(self), level = "debug")]
    async fn execute_parallel_node(&self, parallel: &ParallelNode<'_>) {
        futures_util::future::join_all(
            parallel
                .nodes
                .iter()
                .map(|node| async move { self.execute_node(node).await }),
        )
        .await;
    }

    #[instrument(skip(self), level = "debug")]
    fn execute_introspection_node(&self, introspection: &IntrospectionNode) {
        let value = IntrospectionRoot.resolve(&introspection.selection_set, self.schema);
        let mut current_resp = self.resp.lock();
        merge_data(&mut current_resp.data, value);
    }

    #[instrument(skip(self), level = "debug")]
    async fn execute_fetch_node(&self, fetch: &FetchNode<'_>) {
        let res = self
            .coordinator
            .query(fetch.service, &fetch.query, Default::default())
            .await;
        let mut current_resp = self.resp.lock();

        match res {
            Ok(resp) => {
                if resp.errors.is_empty() {
                    merge_data(&mut current_resp.data, resp.data);
                } else {
                    merge_errors(&mut current_resp.errors, resp.errors);
                }
            }
            Err(err) => current_resp.errors.push(ServerError {
                message: err.to_string(),
                locations: Default::default(),
            }),
        }
    }

    #[instrument(skip(self), level = "debug")]
    async fn execute_flatten_node(&self, flatten: &FlattenNode<'_>) {
        fn extract_keys(from: &mut BTreeMap<Name, ConstValue>, prefix: usize) -> ConstValue {
            let prefix = format!("__key{}_", prefix);
            let mut res = BTreeMap::new();
            let mut keys = Vec::new();
            for key in from.keys() {
                if key.as_str().starts_with(&prefix) {
                    keys.push(key.clone());
                }
            }
            for key in keys {
                if let Some(value) = from.remove(&key) {
                    let name = Name::new(&key[prefix.len()..]);
                    res.insert(name, value);
                }
            }
            ConstValue::Object(res)
        }

        fn get_representations(
            representations: &mut Vec<ConstValue>,
            value: &mut ConstValue,
            path: &[PathSegment<'_>],
            prefix: usize,
        ) {
            let segment = match path.get(0) {
                Some(segment) => segment,
                None => return,
            };
            let is_last = path.len() == 1;

            if is_last {
                match value {
                    ConstValue::Object(object) if !segment.is_list => {
                        if let Some(ConstValue::Object(key_object)) = object.get_mut(segment.name) {
                            representations.push(extract_keys(key_object, prefix));
                        }
                    }
                    ConstValue::Object(object) if segment.is_list => {
                        if let Some(ConstValue::List(array)) = object.get_mut(segment.name) {
                            for element in array {
                                if let ConstValue::Object(element_obj) = element {
                                    representations.push(extract_keys(element_obj, prefix));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            } else {
                match value {
                    ConstValue::Object(object) if !segment.is_list => {
                        if let Some(next_value) = object.get_mut(segment.name) {
                            get_representations(representations, next_value, &path[1..], prefix);
                        }
                    }
                    ConstValue::Object(object) if segment.is_list => {
                        if let Some(ConstValue::List(array)) = object.get_mut(segment.name) {
                            for element in array {
                                get_representations(representations, element, &path[1..], prefix);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        #[inline]
        fn take_value(n: &mut usize, values: &mut [ConstValue]) -> Option<ConstValue> {
            if *n >= values.len() {
                return None;
            }
            let value = std::mem::take(&mut values[*n]);
            *n += 1;
            Some(value)
        }

        fn flatten_values(
            target: &mut ConstValue,
            path: &[PathSegment<'_>],
            n: &mut usize,
            values: &mut [ConstValue],
        ) {
            let segment = match path.get(0) {
                Some(segment) => segment,
                None => return,
            };
            let is_last = path.len() == 1;

            if is_last {
                match target {
                    ConstValue::Object(object) if !segment.is_list => {
                        if let Some(target) = object.get_mut(segment.name) {
                            if let Some(value) = take_value(n, values) {
                                merge_data(target, value);
                            }
                        }
                    }
                    ConstValue::Object(object) if segment.is_list => {
                        if let Some(ConstValue::List(array)) = object.get_mut(segment.name) {
                            for element in array {
                                if let Some(value) = take_value(n, values) {
                                    merge_data(element, value);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            } else {
                match target {
                    ConstValue::Object(object) if !segment.is_list => {
                        if let Some(next_value) = object.get_mut(segment.name) {
                            flatten_values(next_value, &path[1..], n, values);
                        }
                    }
                    ConstValue::Object(object) if segment.is_list => {
                        if let Some(ConstValue::List(array)) = object.get_mut(segment.name) {
                            for element in array {
                                flatten_values(element, &path[1..], n, values);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let representations = {
            let mut representations = Vec::new();
            let mut resp = self.resp.lock();
            get_representations(
                &mut representations,
                &mut resp.data,
                &flatten.path,
                flatten.prefix,
            );
            let mut variables = Variables::default();
            variables.insert(
                Name::new("representations"),
                ConstValue::List(representations),
            );
            variables
        };

        let res = self
            .coordinator
            .query(flatten.service, &flatten.query, representations)
            .await;
        let current_resp = &mut self.resp.lock();

        match res {
            Ok(resp) => {
                if resp.errors.is_empty() {
                    if let ConstValue::Object(mut data) = resp.data {
                        if let Some(ConstValue::List(mut values)) = data.remove("_entities") {
                            let mut n = 0;
                            flatten_values(
                                &mut current_resp.data,
                                &flatten.path,
                                &mut n,
                                &mut values,
                            );
                        }
                    }
                } else {
                    merge_errors(&mut current_resp.errors, resp.errors);
                }
            }
            Err(err) => {
                current_resp.errors.push(ServerError {
                    message: err.to_string(),
                    locations: Default::default(),
                });
            }
        }
    }
}

fn merge_data(target: &mut ConstValue, value: ConstValue) {
    match (target, value) {
        (target @ ConstValue::Null, fragment) => *target = fragment,
        (ConstValue::Object(object), ConstValue::Object(fragment_object)) => {
            for (key, value) in fragment_object {
                match object.get_mut(&key) {
                    Some(target) => merge_data(target, value),
                    None => {
                        object.insert(key, value);
                    }
                }
            }
        }
        (ConstValue::List(array), ConstValue::List(fragment_array))
            if array.len() == fragment_array.len() =>
        {
            for (idx, element) in fragment_array.into_iter().enumerate() {
                merge_data(&mut array[idx], element);
            }
        }
        _ => {}
    }
}

fn merge_errors(target: &mut Vec<ServerError>, errors: Vec<ServerError>) {
    for err in errors {
        target.push(ServerError {
            message: err.message,
            locations: Default::default(),
        })
    }
}
