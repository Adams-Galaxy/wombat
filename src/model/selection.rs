use globset::{GlobBuilder, GlobMatcher};
use std::path::Path;

use crate::model::manifest::{SourceAttribute, SourceComponent, SourceProjection};
use crate::model::path::validate_relative_path;
use crate::{Result, WombatError};

const DOT_PREFIX: &str = "dot_";
const UNALLOC_PREFIX: &str = "unalloc_";
const LITERAL_PREFIX: &str = "literal_";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CompiledSelector {
    pub declared: String,
    pub expanded: String,
    pub physical: String,
    pub hidden: bool,
    pub glob: bool,
    pub static_root: String,
}

pub(crate) fn compile_selector(declared: &str, hidden: bool) -> Result<CompiledSelector> {
    if declared != "." {
        validate_selector_shape(declared)?;
    }
    let expanded = expand_shorthands(declared);
    let physical = if hidden {
        expanded.clone()
    } else {
        encode_target_framed(&expanded)?
    };
    let glob = contains_glob(&physical);
    let static_root = static_root(&physical);
    if hidden {
        validate_hidden_authorization(&physical)?;
    }
    Ok(CompiledSelector {
        declared: declared.to_string(),
        expanded,
        physical,
        hidden,
        glob,
        static_root,
    })
}

pub(crate) fn project_physical(path: &str, hidden_allowed: bool) -> Result<SourceProjection> {
    validate_relative_path(path, "physical source path")?;
    let mut allocated = true;
    let mut hidden = false;
    let mut logical = Vec::new();
    let mut components = Vec::new();
    for component in path.split('/') {
        if component.starts_with('.') {
            hidden = true;
            if !hidden_allowed {
                return Err(WombatError::configuration(format!(
                    "literal hidden source component `{component}` requires w.hidden()"
                )));
            }
            logical.push(component.to_string());
            components.push(SourceComponent {
                physical: component.to_string(),
                logical: component.to_string(),
                attributes: Vec::new(),
            });
            continue;
        }
        let parsed = parse_component(component)?;
        if parsed.attributes.contains(&SourceAttribute::Unallocated) {
            allocated = false;
        }
        logical.push(parsed.logical.clone());
        components.push(parsed);
    }
    Ok(SourceProjection {
        physical: path.to_string(),
        logical: logical.join("/"),
        allocated,
        hidden,
        components,
    })
}

pub(crate) fn encode_target_path(path: &str) -> Result<String> {
    validate_relative_path(path, "target path")?;
    path.split('/')
        .map(encode_target_component)
        .collect::<Result<Vec<_>>>()
        .map(|components| components.join("/"))
}

pub(crate) struct SelectorMatcher(Vec<GlobMatcher>);

impl SelectorMatcher {
    pub fn is_match(&self, path: impl AsRef<Path>) -> bool {
        self.0.iter().any(|matcher| matcher.is_match(path.as_ref()))
    }
}

pub(crate) fn matcher(pattern: &str) -> Result<SelectorMatcher> {
    let patterns = if pattern.contains('/') {
        vec![pattern.to_string()]
    } else {
        vec![
            pattern.to_string(),
            format!("**/{pattern}"),
            format!("{pattern}/**"),
            format!("**/{pattern}/**"),
        ]
    };
    patterns
        .into_iter()
        .map(|pattern| {
            GlobBuilder::new(&pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .map(|glob| glob.compile_matcher())
                .map_err(|error| {
                    WombatError::configuration(format!("invalid source glob: {error}"))
                })
        })
        .collect::<Result<Vec<_>>>()
        .map(SelectorMatcher)
}

pub(crate) fn contains_glob(value: &str) -> bool {
    value
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']'))
}

pub(crate) fn is_hidden_component(component: &str) -> bool {
    component.starts_with('.')
}

pub(crate) fn hidden_components_authorized(path: &str, selector: &str) -> bool {
    let selected = selector.split('/').collect::<Vec<_>>();
    path.split('/').enumerate().all(|(index, component)| {
        !is_hidden_component(component)
            || selected
                .get(index)
                .is_some_and(|selected| *selected == component)
    })
}

pub(crate) fn in_static_scope(path: &str, static_root: &str) -> bool {
    static_root.is_empty()
        || path == static_root
        || path
            .strip_prefix(static_root)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || static_root
            .strip_prefix(path)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(crate) fn is_excluded(exclusions: &[SelectorMatcher], path: &str, directory: bool) -> bool {
    exclusions.iter().any(|matcher| {
        matcher.is_match(path)
            || (directory && matcher.is_match(format!("{path}/__wombat_descendant__")))
    })
}

fn parse_component(component: &str) -> Result<SourceComponent> {
    let physical = component.to_string();
    let mut remainder = component;
    let mut attributes = Vec::new();
    let mut unallocated = false;
    let mut dots = 0usize;
    loop {
        if let Some(value) = remainder.strip_prefix(DOT_PREFIX) {
            attributes.push(SourceAttribute::Dot);
            dots += 1;
            remainder = value;
        } else if let Some(value) = remainder.strip_prefix(UNALLOC_PREFIX) {
            if unallocated {
                return Err(WombatError::configuration(format!(
                    "source component `{component}` repeats non-repeatable `unalloc_` metadata"
                )));
            }
            attributes.push(SourceAttribute::Unallocated);
            unallocated = true;
            remainder = value;
        } else if let Some(value) = remainder.strip_prefix(LITERAL_PREFIX) {
            attributes.push(SourceAttribute::Literal);
            remainder = value;
            break;
        } else {
            break;
        }
    }
    if remainder.is_empty() {
        return Err(WombatError::configuration(format!(
            "source component `{component}` has metadata but no payload name"
        )));
    }
    let logical = format!("{}{}", ".".repeat(dots), remainder);
    if logical == "." || logical == ".." {
        return Err(WombatError::configuration(format!(
            "source component `{component}` resolves to forbidden target component `{logical}`"
        )));
    }
    Ok(SourceComponent {
        physical,
        logical,
        attributes,
    })
}

fn encode_target_framed(value: &str) -> Result<String> {
    if value == "." {
        return Ok(value.to_string());
    }
    value
        .split('/')
        .map(|component| {
            if component.starts_with('.') {
                encode_target_component(component)
            } else {
                Ok(component.to_string())
            }
        })
        .collect::<Result<Vec<_>>>()
        .map(|components| components.join("/"))
}

fn encode_target_component(component: &str) -> Result<String> {
    if component.is_empty() || component == "." || component == ".." {
        return Err(WombatError::configuration(format!(
            "invalid target component `{component}`"
        )));
    }
    let dots = component.bytes().take_while(|byte| *byte == b'.').count();
    let payload = &component[dots..];
    if payload.is_empty() {
        return Err(WombatError::configuration(format!(
            "invalid target component `{component}`"
        )));
    }
    let escaped = if needs_literal(payload) {
        LITERAL_PREFIX
    } else {
        ""
    };
    Ok(format!("{}{escaped}{payload}", DOT_PREFIX.repeat(dots)))
}

fn needs_literal(value: &str) -> bool {
    value.starts_with(DOT_PREFIX)
        || value.starts_with(UNALLOC_PREFIX)
        || value.starts_with(LITERAL_PREFIX)
        || value.contains('@')
}

fn expand_shorthands(value: &str) -> String {
    value
        .split('/')
        .map(|component| {
            let protected = component.find(LITERAL_PREFIX).unwrap_or(component.len());
            let (active, literal) = component.split_at(protected);
            format!("{}{literal}", active.replace('@', UNALLOC_PREFIX))
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn static_root(pattern: &str) -> String {
    let mut components = Vec::new();
    for component in pattern.split('/') {
        if contains_glob(component) {
            break;
        }
        components.push(component);
    }
    components.join("/")
}

fn validate_selector_shape(value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "..")
    {
        return Err(WombatError::configuration(format!(
            "invalid artifact source selector `{value}`"
        )));
    }
    Ok(())
}

fn validate_hidden_authorization(value: &str) -> Result<()> {
    for component in value.split('/') {
        if component.starts_with('.') && contains_glob(component) {
            return Err(WombatError::configuration(format!(
                "w.hidden() must name each literal hidden component explicitly; `{component}` contains glob syntax"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        compile_selector, encode_target_path, hidden_components_authorized, matcher,
        project_physical,
    };
    use crate::model::manifest::SourceAttribute;

    #[test]
    fn metadata_composes_and_literal_stops_parsing() {
        let projected = project_physical("unalloc_dot_config/literal_dot_file", false).unwrap();
        assert!(!projected.allocated);
        assert_eq!(projected.logical, ".config/dot_file");
        assert_eq!(
            projected.components[0].attributes,
            [SourceAttribute::Unallocated, SourceAttribute::Dot]
        );
        assert!(project_physical("unalloc_unalloc_file", false).is_err());
    }

    #[test]
    fn shorthand_is_textual_before_globbing() {
        let selector = compile_selector("**@**", false).unwrap();
        assert_eq!(selector.physical, "**unalloc_**");
        assert!(selector.glob);
        assert!(
            matcher(&selector.physical)
                .unwrap()
                .is_match("nested/unalloc_examples/file")
        );
    }

    #[test]
    fn target_paths_encode_metadata_like_literals() {
        assert_eq!(encode_target_path(".config/app").unwrap(), "dot_config/app");
        assert_eq!(
            encode_target_path("dot_config/@file").unwrap(),
            "literal_dot_config/literal_@file"
        );
    }

    #[test]
    fn hidden_selectors_are_explicit() {
        let hidden = compile_selector(".external/file", true).unwrap();
        assert_eq!(hidden.physical, ".external/file");
        let ordinary = compile_selector(".external/file", false).unwrap();
        assert_eq!(ordinary.physical, "dot_external/file");
        assert!(compile_selector(".*", true).is_err());
        assert!(hidden_components_authorized(
            ".external/file",
            ".external/**"
        ));
        assert!(!hidden_components_authorized(
            ".external/.secret",
            ".external/**"
        ));
    }

    #[test]
    fn metadata_projection_is_stable_across_composed_prefixes() {
        for (physical, logical, allocated) in [
            ("dot_config", ".config", true),
            ("unalloc_config", "config", false),
            ("unalloc_dot_config", ".config", false),
            ("literal_dot_config", "dot_config", true),
            ("dot_literal_dot_config", ".dot_config", true),
        ] {
            let first = project_physical(physical, false).unwrap();
            let second = project_physical(physical, false).unwrap();
            assert_eq!(
                first, second,
                "projection must be deterministic for {physical}"
            );
            assert_eq!(first.logical, logical);
            assert_eq!(first.allocated, allocated);
        }
    }

    #[test]
    fn selector_globs_never_authorize_hidden_components_implicitly() {
        for pattern in ["*", "**", "dir/**", "**/*.toml", "**@**"] {
            let selector = compile_selector(pattern, false).unwrap();
            assert!(!hidden_components_authorized(
                ".hidden/value",
                &selector.physical
            ));
        }
    }
}
