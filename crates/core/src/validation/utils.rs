use std::collections::HashSet;
use std::fmt::{Display, Formatter, Result as FmtResult};

use parser::types::{BaseType, Type};
use value::{ConstValue, Value};

use crate::schema::TypeKind;
use crate::ComposedSchema;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Scope<'a> {
    Operation(Option<&'a str>),
    Fragment(&'a str),
}

#[derive(Debug, Copy, Clone)]
pub enum PathSegment<'a> {
    Name(&'a str),
    Index(usize),
}

#[derive(Debug, Copy, Clone)]
pub struct PathNode<'a> {
    pub parent: Option<&'a PathNode<'a>>,
    pub segment: PathSegment<'a>,
}

impl<'a> PathNode<'a> {
    pub fn new(name: &'a str) -> Self {
        PathNode {
            parent: None,
            segment: PathSegment::Name(name),
        }
    }

    pub fn index(&'a self, idx: usize) -> Self {
        Self {
            parent: Some(self),
            segment: PathSegment::Index(idx),
        }
    }

    pub fn name(&'a self, name: &'a str) -> Self {
        Self {
            parent: Some(self),
            segment: PathSegment::Name(name),
        }
    }
}

impl<'a> Display for PathNode<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        fn write_node(f: &mut Formatter<'_>, node: &PathNode) -> FmtResult {
            if let Some(parent) = node.parent {
                write_node(f, parent)?;
                write!(f, ".")?;
            }
            match &node.segment {
                PathSegment::Name(name) => write!(f, "{}", name),
                PathSegment::Index(idx) => write!(f, "{}", idx),
            }
        }
        write_node(f, self)
    }
}

pub fn referenced_variables(value: &Value) -> Vec<&str> {
    fn referenced_variables_to_vec<'a>(value: &'a Value, vars: &mut Vec<&'a str>) {
        match value {
            Value::Variable(name) => {
                vars.push(name);
            }
            Value::List(values) => values
                .iter()
                .for_each(|value| referenced_variables_to_vec(value, vars)),
            Value::Object(obj) => obj
                .values()
                .for_each(|value| referenced_variables_to_vec(value, vars)),
            _ => {}
        }
    }

    let mut vars = Vec::new();
    referenced_variables_to_vec(value, &mut vars);
    vars
}

fn valid_error(path_node: &PathNode, msg: String) -> String {
    format!("\"{}\", {}", path_node, msg)
}

pub fn is_valid_input_value(
    schema: &ComposedSchema,
    ty: &Type,
    value: &ConstValue,
    path_node: PathNode,
) -> Option<String> {
    fn is_valid_input_base_value(
        schema: &ComposedSchema,
        base_ty: &BaseType,
        value: &ConstValue,
        path_node: PathNode,
    ) -> Option<String> {
        match &base_ty {
            BaseType::List(element_ty) => match value {
                ConstValue::List(elements) => {
                    elements.iter().enumerate().find_map(|(idx, elem)| {
                        is_valid_input_value(schema, element_ty, elem, path_node.index(idx))
                    })
                }
                ConstValue::Null => None,
                _ => is_valid_input_value(schema, element_ty, value, path_node),
            },
            BaseType::Named(type_name) => {
                if matches!(value, ConstValue::Null) {
                    return None;
                }
                if let Some(ty) = schema.types.get(type_name) {
                    match ty.kind {
                        TypeKind::Scalar => {
                            if is_valid_scalar_value(ty.name.as_str(), value) {
                                None
                            } else {
                                Some(valid_error(
                                    &path_node,
                                    format!("expected type \"{}\"", type_name),
                                ))
                            }
                        }
                        TypeKind::Enum => {
                            if let ConstValue::Enum(value) = value {
                                if !ty.enum_values.contains_key(value) {
                                    Some(valid_error(
                                        &path_node,
                                        format!(
                                            "enumeration type \"{}\" does not contain the value \"{}\"",
                                            ty.name, value
                                        ),
                                    ))
                                } else {
                                    None
                                }
                            } else {
                                Some(valid_error(
                                    &path_node,
                                    format!("expected type \"{}\"", type_name),
                                ))
                            }
                        }
                        TypeKind::InputObject => {
                            if let ConstValue::Object(values) = value {
                                let mut input_names = values.keys().collect::<HashSet<_>>();

                                for field in ty.input_fields.values() {
                                    input_names.remove(&field.name);
                                    if let Some(value) = values.get(&field.name) {
                                        if let Some(reason) = is_valid_input_value(
                                            schema,
                                            &field.ty,
                                            value,
                                            path_node.name(field.name.as_str()),
                                        ) {
                                            return Some(reason);
                                        }
                                    } else if !field.ty.nullable && field.default_value.is_none() {
                                        return Some(valid_error(
                                            &path_node,
                                            format!(
                                                "field \"{}\" of type \"{}\" is required but not provided",
                                                field.name, ty.name,
                                            ),
                                        ));
                                    }
                                }

                                if let Some(name) = input_names.iter().next() {
                                    return Some(valid_error(
                                        &path_node,
                                        format!(
                                            "unknown field \"{}\" of type \"{}\"",
                                            name, ty.name
                                        ),
                                    ));
                                }

                                None
                            } else {
                                Some(valid_error(
                                    &path_node,
                                    format!("expected type \"{}\"", type_name),
                                ))
                            }
                        }
                        _ => None,
                    }
                } else {
                    None
                }
            }
        }
    }

    if !ty.nullable {
        return if matches!(value, ConstValue::Null) {
            Some(valid_error(&path_node, format!("expected type \"{}\"", ty)))
        } else {
            is_valid_input_base_value(schema, &ty.base, value, path_node)
        };
    } else {
        is_valid_input_base_value(schema, &ty.base, value, path_node)
    }
}

fn is_valid_scalar_value(type_name: &str, value: &ConstValue) -> bool {
    match (type_name, value) {
        ("Int", ConstValue::Number(n)) if n.is_i64() || n.is_u64() => true,
        ("Float", ConstValue::Number(_)) => true,
        ("String", ConstValue::String(_)) => true,
        ("Boolean", ConstValue::Boolean(_)) => true,
        ("ID", ConstValue::String(_)) => true,
        ("ID", ConstValue::Number(n)) if n.is_i64() || n.is_u64() => true,
        _ => false,
    }
}
