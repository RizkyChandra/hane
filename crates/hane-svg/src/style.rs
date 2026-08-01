//! Presentation attributes and inline `style`, resolved into computed values.
//!
//! An SVG element's appearance comes from three places at once: what it
//! declares as a presentation attribute (`fill="red"`), what it declares in its
//! `style` attribute (`style="fill: red"`), and what it inherits from its
//! parent. [`Style::resolve`] walks one element and produces the computed value
//! of every property this crate models, so a renderer reads a value and never a
//! rule.
//!
//! Values stay as strings. Turning `red`, `#f00` and `url(#g)` into paint is a
//! separate problem from deciding *which* string wins, and the crate has no
//! colour type yet.
//!
//! # What this deliberately does not do
//!
//! - **No CSS stylesheets.** No `<style>` element, no selectors, no cascade
//!   beyond the two sources above -- so `!important` is stripped rather than
//!   honoured, having nothing left to outrank.
//! - **No value validation.** An unparseable `stroke-width: banana` computes to
//!   `banana`; whoever consumes the value decides what to do with it, since
//!   only they know the property's grammar.

use crate::xml::Element;

/// Every property this crate models: its name, whether it inherits, and its
/// initial value.
///
/// Inheritance is per-property and not guessable -- `fill` inherits, `opacity`
/// does not, and a group's 50% opacity applying once to the group rather than
/// separately to each child is the visible difference.
const PROPERTIES: &[(&str, bool, &str)] = &[
    // `color` is first only because it is cheap to find; `currentColor`
    // resolves against it, so it is computed before the properties that
    // reference it.
    ("color", true, "black"),
    ("fill", true, "black"),
    ("fill-opacity", true, "1"),
    ("fill-rule", true, "nonzero"),
    ("stroke", true, "none"),
    ("stroke-width", true, "1"),
    ("stroke-linecap", true, "butt"),
    ("stroke-linejoin", true, "miter"),
    ("stroke-miterlimit", true, "4"),
    ("stroke-dasharray", true, "none"),
    ("stroke-dashoffset", true, "0"),
    ("stroke-opacity", true, "1"),
    ("visibility", true, "visible"),
    ("opacity", false, "1"),
    ("display", false, "inline"),
    ("stop-color", false, "black"),
    ("stop-opacity", false, "1"),
];

/// The index of `color` in [`PROPERTIES`], for `currentColor` resolution.
const COLOR: usize = 0;

/// The computed value of every property in [`PROPERTIES`], for one element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Style {
    /// Parallel to [`PROPERTIES`], and always the same length.
    values: Vec<String>,
}

impl Style {
    /// The initial value of every property: what the root element inherits
    /// from.
    pub fn initial() -> Self {
        Self {
            values: PROPERTIES.iter().map(|&(_, _, v)| v.to_string()).collect(),
        }
    }

    /// The computed style of `element`, whose parent computed to `self`.
    ///
    /// Precedence, highest first: the `style` attribute, then the matching
    /// presentation attribute, then the parent's computed value for an
    /// inheriting property or the initial value for one that does not.
    pub fn resolve(&self, element: &Element) -> Self {
        let declarations = parse_declarations(element.attribute("style").unwrap_or(""));
        let mut values: Vec<String> = PROPERTIES
            .iter()
            .enumerate()
            .map(|(i, &(name, inherited, initial))| {
                // Last declaration wins within the attribute, as in CSS.
                let declared = declarations
                    .iter()
                    .rev()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| *value)
                    .or_else(|| element.attribute(name));
                match declared {
                    // `inherit` takes the parent's *computed* value even for a
                    // property that does not normally inherit.
                    Some(v) if v.eq_ignore_ascii_case("inherit") => self.values[i].clone(),
                    Some(v) => v.to_string(),
                    None if inherited => self.values[i].clone(),
                    None => initial.to_string(),
                }
            })
            .collect();

        // `color: currentColor` is circular, so it means the inherited colour;
        // everything else resolves against this element's own computed colour.
        if values[COLOR].eq_ignore_ascii_case(CURRENT_COLOR) {
            values[COLOR].clone_from(&self.values[COLOR]);
        }
        for i in 0..values.len() {
            if i != COLOR && values[i].eq_ignore_ascii_case(CURRENT_COLOR) {
                values[i] = values[COLOR].clone();
            }
        }
        Self { values }
    }

    /// The computed value of `property`, or `None` when it is not a property
    /// this crate models.
    pub fn get(&self, property: &str) -> Option<&str> {
        let i = PROPERTIES
            .iter()
            .position(|&(name, _, _)| name == property)?;
        Some(&self.values[i])
    }
}

/// The keyword, in the spelling the spec uses. Matched case-insensitively:
/// it is a CSS keyword, and CSS keywords are case-insensitive wherever they
/// appear -- including in a presentation attribute.
const CURRENT_COLOR: &str = "currentColor";

/// Split a `style` attribute into `(property, value)` pairs, in source order.
///
/// Malformed declarations are dropped rather than rejected: CSS error recovery
/// discards the bad declaration and keeps the rest, and an SVG that renders
/// mostly right beats one that fails to open.
fn parse_declarations(attribute: &str) -> Vec<(&str, &str)> {
    // ponytail: no CSS comments and no quoted-string awareness, so a `;` or `:`
    // inside `font-family: "a;b"` splits wrongly. Lift this to a real
    // declaration tokeniser if `<style>` elements ever land.
    attribute
        .split(';')
        .filter_map(|declaration| {
            let (name, value) = declaration.split_once(':')?;
            let value = match value.rsplit_once('!') {
                Some((head, tail)) if tail.trim().eq_ignore_ascii_case("important") => head,
                _ => value,
            };
            let (name, value) = (name.trim(), value.trim());
            (!name.is_empty() && !value.is_empty()).then_some((name, value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xml::{Node, parse};

    /// The computed style of `<g>`, and of the `<path>` nested inside it.
    fn resolve(group: &str, child: &str) -> (Style, Style) {
        let root = parse(&format!("<g {group}><path {child}/></g>")).expect("should parse");
        let parent = Style::initial().resolve(&root);
        let Node::Element(path) = &root.children[0] else {
            panic!("expected the path child");
        };
        let child = parent.resolve(path);
        (parent, child)
    }

    fn get(style: &Style, property: &str) -> String {
        style
            .get(property)
            .expect("a modelled property")
            .to_string()
    }

    #[test]
    fn unset_properties_take_their_initial_value() {
        let (_, child) = resolve("", "");
        assert_eq!(get(&child, "fill"), "black");
        assert_eq!(get(&child, "stroke"), "none");
        assert_eq!(get(&child, "fill-rule"), "nonzero");
        assert_eq!(child.get("font-size"), None);
    }

    #[test]
    fn inline_style_beats_the_presentation_attribute() {
        let (parent, _) = resolve("fill='red' style='fill: blue'", "");
        assert_eq!(get(&parent, "fill"), "blue");
    }

    #[test]
    fn a_presentation_attribute_still_applies_when_style_is_silent_about_it() {
        let (parent, _) = resolve("fill='red' style='stroke: blue'", "");
        assert_eq!(get(&parent, "fill"), "red");
        assert_eq!(get(&parent, "stroke"), "blue");
    }

    #[test]
    fn inheritable_properties_inherit_and_others_do_not() {
        let (_, child) = resolve("fill='red' opacity='0.5' display='none'", "");
        assert_eq!(get(&child, "fill"), "red");
        // Group opacity applies to the composited group, so a child that says
        // nothing is fully opaque, not half.
        assert_eq!(get(&child, "opacity"), "1");
        assert_eq!(get(&child, "display"), "inline");
    }

    #[test]
    fn the_child_overrides_what_it_inherits() {
        let (_, child) = resolve("fill='red'", "fill='green'");
        assert_eq!(get(&child, "fill"), "green");
    }

    #[test]
    fn inherit_pulls_the_parent_value_through_for_any_property() {
        let (_, child) = resolve(
            "fill='red' opacity='0.5'",
            "fill='inherit' opacity='inherit'",
        );
        assert_eq!(get(&child, "fill"), "red");
        // Explicit `inherit` reaches a property that never inherits on its own.
        assert_eq!(get(&child, "opacity"), "0.5");
    }

    #[test]
    fn inherit_on_the_root_reaches_the_initial_value() {
        let (parent, _) = resolve("fill='inherit'", "");
        assert_eq!(get(&parent, "fill"), "black");
    }

    #[test]
    fn current_color_resolves_against_the_computed_color() {
        let (parent, child) = resolve("color='red' fill='currentColor'", "stroke='currentColor'");
        assert_eq!(get(&parent, "fill"), "red");
        // `color` inherits, so the child's `currentColor` is the parent's red.
        assert_eq!(get(&child, "stroke"), "red");

        // The element's own `color` wins over the inherited one, whichever
        // order the two declarations appear in.
        let (_, child) = resolve("color='red'", "style='fill: currentColor; color: blue'");
        assert_eq!(get(&child, "fill"), "blue");
    }

    #[test]
    fn current_color_on_a_non_inheriting_property_still_works() {
        let (_, child) = resolve("color='red'", "stop-color='currentColor'");
        assert_eq!(get(&child, "stop-color"), "red");
    }

    #[test]
    fn color_set_to_current_color_is_the_inherited_color() {
        // Circular by the letter of the spec, defined to mean `inherit`.
        let (_, child) = resolve("color='red'", "color='currentColor' fill='currentColor'");
        assert_eq!(get(&child, "color"), "red");
        assert_eq!(get(&child, "fill"), "red");
    }

    #[test]
    fn keywords_are_case_insensitive() {
        let (_, child) = resolve("color='red' fill='blue'", "fill='CURRENTCOLOR'");
        assert_eq!(get(&child, "fill"), "red");
        let (_, child) = resolve("fill='blue'", "style='FILL: INHERIT'");
        assert_eq!(get(&child, "fill"), "blue");
    }

    #[test]
    fn declaration_syntax_is_forgiving() {
        let (parent, _) = resolve(
            "style='  fill : red ;; nonsense ; stroke:;stroke-width:2!important ; fill: green '",
            "",
        );
        // Last declaration of a property wins.
        assert_eq!(get(&parent, "fill"), "green");
        // A value-less declaration is dropped, not treated as empty.
        assert_eq!(get(&parent, "stroke"), "none");
        assert_eq!(get(&parent, "stroke-width"), "2");
    }

    #[test]
    fn unknown_declarations_are_ignored() {
        let (parent, _) = resolve("style='fill: red; -webkit-wobble: 3'", "");
        assert_eq!(get(&parent, "fill"), "red");
        assert_eq!(parent.get("-webkit-wobble"), None);
    }
}
