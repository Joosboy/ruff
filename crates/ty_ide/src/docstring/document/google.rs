use std::cmp::Ordering;

use indexmap::IndexMap;
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_text_size::{TextRange, TextSize};

use super::SectionKind;
use super::preformatted::PreformattedBlockScanner;
use super::syntax::{
    ParsedLine, container_block_end, parse_parenthesized_type, parsed_lines,
    split_once_unbracketed_colon,
};

/// Returns parameter documentation from recognized Google-style parameter sections.
pub(super) fn parameter_documentation(raw: &str) -> IndexMap<String, String> {
    let mut parameters = IndexMap::new();
    visit_sections(raw, |kind, body, _, _| {
        if matches!(
            kind,
            SectionKind::Parameters | SectionKind::KeywordArguments | SectionKind::OtherParameters
        ) {
            extend_parameter_documentation(&mut parameters, body);
        }
    });
    parameters
}

/// Visits recognized Google-style sections in source order.
///
/// For each section, `visit` receives its kind, body lines, full source range (including the
/// header), and header indentation.
pub(in crate::docstring) fn visit_sections<'a>(
    raw: &'a str,
    mut visit: impl FnMut(SectionKind, &[ParsedLine<'a>], TextRange, TextSize),
) {
    let lines = parsed_lines(raw);
    let mut preformatted_blocks = PreformattedBlockScanner::default();
    let mut index = 0;

    while index < lines.len() {
        // Skip blocks that "own" all internal content (in which we should not
        // recognize content that might otherwise look like a Google section header)
        if preformatted_blocks.consume_preformatted_line(lines[index].text) {
            index += 1;
            continue;
        }
        if let Some(end) = container_block_end(&lines, index) {
            index = end;
            continue;
        }

        let Some(header) = parse_section_header(&lines, index) else {
            preformatted_blocks.observe_line_outside_preformatted_block(lines[index].text);
            index += 1;
            continue;
        };

        let (range, body_end_line_index) = section_body_end(&lines, header);
        if let HeaderKind::Structured(kind) = header.kind {
            visit(
                kind,
                &lines[header.body_start_line_index..body_end_line_index],
                range,
                header.indent,
            );
        }
        index = body_end_line_index;
    }
}

/// Extends `parameters` with the documented items in one parameter section body.
fn extend_parameter_documentation(
    parameters: &mut IndexMap<String, String>,
    lines: &[ParsedLine<'_>],
) {
    let mut current: Option<(String, String)> = None;
    let mut item_indent = None;

    for line in lines {
        let trimmed = line.text.trim();

        // The first recognized item establishes the sibling indentation.
        // Each item at that indentation starts a new sibling and completes its predecessor.
        if item_indent.is_none_or(|indent| line.raw_indent == indent)
            && let Some((names, description)) = parse_parameter(trimmed)
        {
            insert_parameter_documentation(
                parameters,
                current.replace((names.to_string(), description.to_string())),
            );
            item_indent.get_or_insert(line.raw_indent);
            continue;
        }

        // Ignore prose until the first item has started.
        let Some((_, description)) = &mut current else {
            continue;
        };

        // Lines that are not sibling items extend the current description.
        // Empty lines preserve paragraph breaks.
        if !description.is_empty() && !description.ends_with('\n') {
            description.push('\n');
        }
        description.push_str(if trimmed.is_empty() { "\n" } else { trimmed });
    }

    // A following item completes its predecessor in the loop, so complete the final item here.
    insert_parameter_documentation(parameters, current);
}

/// Parses a parameter item into its display name and description.
fn parse_parameter(line: &str) -> Option<(&str, &str)> {
    let (name, description) = split_once_unbracketed_colon(line)?;
    let (display_name, _) = parse_parenthesized_type(name.trim());

    google_parameter_names(display_name)
        .all(is_parameter_name)
        .then_some((display_name, description.trim()))
}

/// Returns whether `name` is a valid Python parameter name, including variadic prefixes.
pub(in crate::docstring) fn is_parameter_name(name: &str) -> bool {
    let identifier = name
        .strip_prefix("**")
        .or_else(|| name.strip_prefix('*'))
        .unwrap_or(name);
    is_identifier(identifier)
}

/// Inserts a completed parameter item under each of its comma-separated names.
fn insert_parameter_documentation(
    parameters: &mut IndexMap<String, String>,
    parameter: Option<(String, String)>,
) {
    let Some((names, description)) = parameter else {
        return;
    };
    let description = description.trim();
    if !description.is_empty() {
        for name in google_parameter_names(&names) {
            parameters.insert(name.to_string(), description.to_string());
        }
    }
}

fn google_parameter_names(display_name: &str) -> impl Iterator<Item = &str> {
    display_name.split(',').map(str::trim)
}

/// Parses a recognized Google-style section header at `index`.
fn parse_section_header(lines: &[ParsedLine<'_>], index: usize) -> Option<SectionHeader> {
    let line = lines.get(index)?;
    let kind = section_kind(line.text)?;

    Some(SectionHeader {
        kind,
        indent: line.raw_indent,
        structural_indent: line.structural_indent,
        body_start_line_index: index + 1,
        range: line.range,
    })
}

fn section_kind(line: &str) -> Option<HeaderKind> {
    let name = line.trim().strip_suffix(':')?.trim();
    section_kind_from_name(name)
}

fn section_kind_from_name(name: &str) -> Option<HeaderKind> {
    let normalized = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    Some(match normalized.as_str() {
        "args" | "arguments" | "parameters" => HeaderKind::Structured(SectionKind::Parameters),
        "keyword args" | "keyword arguments" => {
            HeaderKind::Structured(SectionKind::KeywordArguments)
        }
        "other args" | "other arguments" | "other parameters" => {
            HeaderKind::Structured(SectionKind::OtherParameters)
        }
        "attributes" => HeaderKind::Structured(SectionKind::Attributes),
        "return" | "returns" => HeaderKind::Structured(SectionKind::Returns),
        "yield" | "yields" => HeaderKind::Structured(SectionKind::Yields),
        "raise" | "raises" => HeaderKind::Structured(SectionKind::Raises),
        "attention" | "caution" | "danger" | "error" | "example" | "examples" | "hint"
        | "important" | "methods" | "note" | "notes" | "references" | "see also" | "tip"
        | "todo" | "todos" | "warning" | "warnings" | "warns" => HeaderKind::Container,
        _ => return None,
    })
}

/// Returns the section's source range and the index of the first line outside its body.
fn section_body_end(lines: &[ParsedLine<'_>], header: SectionHeader) -> (TextRange, usize) {
    let mut body_end_index = header.body_start_line_index;
    let mut preformatted_blocks = PreformattedBlockScanner::default();
    let mut item_indent = None;

    while let Some(line) = lines.get(body_end_index) {
        // Once a preformatted block begins, its contents cannot end the section.
        if preformatted_blocks.is_active()
            && preformatted_blocks.consume_preformatted_line(line.text)
        {
            body_end_index += 1;
            continue;
        }

        let Some((leading_blank_lines, line)) =
            section_body_continuation(&lines[body_end_index..], header, item_indent)
        else {
            break;
        };
        body_end_index += leading_blank_lines;

        item_indent = item_indent.or_else(|| section_item_indent(header, line));

        if !preformatted_blocks.consume_preformatted_line(line.text) {
            preformatted_blocks.observe_line_outside_preformatted_block(line.text);
        }
        body_end_index += 1;
    }

    let body = &lines[header.body_start_line_index..body_end_index];
    let range = match body.last() {
        Some(last) => header.range.cover(last.range),
        None => header.range,
    };
    (range, body_end_index)
}

/// Returns the number of leading blank lines and first nonblank line that continue
/// `header`'s body.
fn section_body_continuation<'a>(
    lines: &[ParsedLine<'a>],
    header: SectionHeader,
    item_indent: Option<TextSize>,
) -> Option<(usize, ParsedLine<'a>)> {
    let (leading_blank_lines, next_line) = lines
        .iter()
        .enumerate()
        .find(|(_, line)| !line.text.trim().is_empty())?;

    if leading_blank_lines == 0 && section_header_ends_body(lines, 0, header) {
        return None;
    }

    if leading_blank_lines > 0
        && next_line.structural_indent <= header.structural_indent
        && (parse_section_header(lines, leading_blank_lines).is_some()
            || is_inline_section_header(next_line.text))
    {
        return None;
    }

    // Returns and yields have no item syntax that distinguishes an aligned body from prose
    // following an empty section.
    if leading_blank_lines > 0
        && next_line.raw_indent <= header.indent
        && item_indent.is_none()
        && matches!(
            header.kind,
            HeaderKind::Structured(SectionKind::Returns | SectionKind::Yields)
        )
    {
        return None;
    }

    // A blank line ends a parameter section when the following aligned text is
    // not another parameter item.
    if leading_blank_lines > 0
        && matches!(
            header.kind,
            HeaderKind::Structured(
                SectionKind::Parameters
                    | SectionKind::KeywordArguments
                    | SectionKind::OtherParameters
            )
        )
        && item_indent == Some(next_line.raw_indent)
        && section_item_indent(header, *next_line).is_none()
    {
        return None;
    }

    line_belongs_to_body(header, *next_line, item_indent)
        .then_some((leading_blank_lines, *next_line))
}

/// Returns whether a recognized header at `index` ends the current section body.
fn section_header_ends_body(lines: &[ParsedLine<'_>], index: usize, header: SectionHeader) -> bool {
    let Some(line) = lines.get(index) else {
        return false;
    };
    if line.structural_indent <= header.structural_indent && is_inline_section_header(line.text) {
        return true;
    }

    parse_section_header(lines, index)
        .is_some_and(|next| next.structural_indent <= header.structural_indent)
}

/// Returns whether `line` belongs to `header` under Google-style indentation rules.
fn line_belongs_to_body(
    header: SectionHeader,
    line: ParsedLine<'_>,
    item_indent: Option<TextSize>,
) -> bool {
    match line.raw_indent.cmp(&header.indent) {
        Ordering::Less => false,
        Ordering::Greater => true,
        Ordering::Equal => {
            let item_indent_matches_line =
                item_indent.is_none_or(|indent| indent == line.raw_indent);
            let is_parameter_section = matches!(
                header.kind,
                HeaderKind::Structured(
                    SectionKind::Parameters
                        | SectionKind::KeywordArguments
                        | SectionKind::OtherParameters
                )
            );

            // Parameter sections can start with aligned prose before an item establishes the
            // sibling indentation. Once established, aligned lines must match that indentation.
            item_indent_matches_line
                && (is_parameter_section || section_item_indent(header, line).is_some())
        }
    }
}

/// Returns the indentation of an item recognized in the current section.
///
/// The first recognized item establishes the indentation for sibling items.
/// Item-like lines at a different indentation within the section are treated as
/// continuation text.
fn section_item_indent(header: SectionHeader, line: ParsedLine<'_>) -> Option<TextSize> {
    let trimmed = line.text.trim();
    let is_item = match header.kind {
        HeaderKind::Structured(
            SectionKind::Parameters | SectionKind::KeywordArguments | SectionKind::OtherParameters,
        ) => parse_parameter(trimmed).is_some(),
        HeaderKind::Structured(SectionKind::Attributes | SectionKind::Raises) => {
            split_once_unbracketed_colon(trimmed).is_some_and(|(name, _)| !name.trim().is_empty())
        }
        HeaderKind::Structured(SectionKind::Returns | SectionKind::Yields) => !trimmed.is_empty(),
        HeaderKind::Container => false,
    };
    is_item.then_some(line.raw_indent)
}

/// Returns whether `line` is a recognized section header followed by inline content.
fn is_inline_section_header(line: &str) -> bool {
    let line = line.trim();
    // A trailing double colon introduces a reST literal block, not an inline section.
    if line.ends_with("::") {
        return false;
    }

    let Some((name, description)) = split_once_unbracketed_colon(line) else {
        return false;
    };

    let name = name.trim();
    let description = description.trim();
    !description.is_empty()
        && name.chars().next().is_some_and(char::is_uppercase)
        && section_kind_from_name(name).is_some()
}

/// Returns whether `line` is a recognized Google-style section header.
pub(in crate::docstring) fn is_section_like_header(line: &str) -> bool {
    section_kind(line).is_some() || is_inline_section_header(line)
}

/// Returns whether `name` ends with a conventional exception-class suffix.
pub(in crate::docstring) fn has_exception_name_suffix(name: &str) -> bool {
    ["Error", "Exception", "Warning"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// Returns whether every component of `name` is a Python identifier.
pub(in crate::docstring) fn is_dotted_identifier(name: &str) -> bool {
    !name.is_empty() && name.split('.').all(is_identifier)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SectionHeader {
    kind: HeaderKind,
    indent: TextSize,
    structural_indent: TextSize,
    body_start_line_index: usize,
    range: TextRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderKind {
    Structured(SectionKind),
    Container,
}

#[cfg(test)]
mod tests {
    use ruff_text_size::TextSize;

    use super::{SectionKind, parameter_documentation, visit_sections};

    #[test]
    fn extracts_parameter_items() {
        for (raw, expected) in [
            (
                "\
Arguments:
first: First parameter.
Aligned continuation.
second: Second parameter.
Returns:
bool: Result.",
                &[
                    ("first", "First parameter.\nAligned continuation."),
                    ("second", "Second parameter."),
                ][..],
            ),
            (
                "\
Args:
  \tfirst: First parameter.
        second: Second parameter.",
                &[
                    ("first", "First parameter."),
                    ("second", "Second parameter."),
                ],
            ),
            (
                "\
Args:
    x, y: Coordinates.",
                &[("x", "Coordinates."), ("y", "Coordinates.")],
            ),
            (
                "\
Args:
Partition into non-overlapping windows with padding if needed.
    hidden_states (tensor): Input tokens.",
                &[("hidden_states", "Input tokens.")],
            ),
            (
                "\
Args:
    query_embeddings (`Union[torch.Tensor, list[torch.Tensor]`): Query embeddings.",
                &[("query_embeddings", "Query embeddings.")],
            ),
            (
                "\
Args:
----
    value: Parameter documentation.",
                &[("value", "Parameter documentation.")],
            ),
            (
                "\
Args:
    value: Initial documentation.
    value, for example: can be omitted.",
                &[(
                    "value",
                    "Initial documentation.\nvalue, for example: can be omitted.",
                )],
            ),
            (
                "\
Args:
    value: First documentation.
    value: Replacement documentation.",
                &[("value", "Replacement documentation.")],
            ),
            (
                "\
Args:
    value (Literal[\"(\"]): Quoted parenthesis.",
                &[("value", "Quoted parenthesis.")],
            ),
            (
                "\
Args:
    callback() (Callable): Not a parameter.
    value: Documentation.",
                &[("value", "Documentation.")],
            ),
            (
                "\
Args:
    value: First paragraph.


        Second paragraph.",
                &[("value", "First paragraph.\n\n\nSecond paragraph.")],
            ),
        ] {
            assert_parameter_documentation(raw, expected);
        }
    }

    #[test]
    fn recognizes_parameter_section_headings() {
        for heading in [
            "Args",
            "Arguments",
            "Parameters",
            "Keyword Args",
            "Keyword Arguments",
            "Other Args",
            "Other Arguments",
            "Other Parameters",
        ] {
            let raw = format!(
                "\
{heading}:
    value: Parameter documentation."
            );
            assert_parameter_documentation(&raw, &[("value", "Parameter documentation.")]);
        }
    }

    #[test]
    fn respects_section_boundaries() {
        for (raw, expected) in [
            (
                "\
Args:
    value: Parameter documentation.
Methods:
    helper: Method documentation.",
                &[("value", "Parameter documentation.")][..],
            ),
            (
                "\
Example:
    Args:
        nested: Not parameter documentation.
Args:
    value: Parameter documentation.",
                &[("value", "Parameter documentation.")],
            ),
            (
                "\
Args:
    first: First parameter.
    last: Last parameter.

Returns: Result.",
                &[("first", "First parameter."), ("last", "Last parameter.")],
            ),
            (
                "\
Args:
value: Parameter documentation.

Additional details.",
                &[("value", "Parameter documentation.")],
            ),
            (
                "\
Args:
first: First parameter.
last: Last parameter.
Returns: Result.",
                &[("first", "First parameter."), ("last", "Last parameter.")],
            ),
            (
                "\
Args:
    value: Parameter documentation.

    Additional details.",
                &[("value", "Parameter documentation.")],
            ),
        ] {
            assert_parameter_documentation(raw, expected);
        }
    }

    #[test]
    fn uses_pep257_indentation_for_section_hierarchy() {
        assert_parameter_documentation(
            "\
Note:
        context

    Args:
        value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
        assert_parameter_documentation(
            "
    Note:
        context

        Args:
            nested: Not parameter documentation.",
            &[],
        );
        assert_parameter_documentation(
            "\
Example:
Args:
    value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
        assert_parameter_documentation(
            "\
Args:
        value: Parameter documentation.
    Returns:
        bool: Result.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn finds_shifted_top_level_section() {
        assert_parameter_documentation(
            "\
A decoded newline follows:
This line starts at column zero.

    Keyword Args:
        shifted: Documentation in a shifted section.",
            &[("shifted", "Documentation in a shifted section.")],
        );
    }

    #[test]
    fn keeps_colon_prose_in_parameter_documentation() {
        assert_parameter_documentation(
            "\
Args:
    param1 (str): The first parameter description.
    For example: pass an absolute path.
    param2: The second parameter description.",
            &[
                (
                    "param1",
                    "The first parameter description.\nFor example: pass an absolute path.",
                ),
                ("param2", "The second parameter description."),
            ],
        );
    }

    #[test]
    fn keeps_rest_literal_blocks_in_parameter_documentation() {
        assert_parameter_documentation(
            "\
Args:
    value: Documentation.
        Example::
            Args:
                nested: Not parameter documentation.
    other: Other documentation.",
            &[
                (
                    "value",
                    "Documentation.\nExample::\nArgs:\nnested: Not parameter documentation.",
                ),
                ("other", "Other documentation."),
            ],
        );
    }

    #[test]
    fn extracts_variadic_parameters() {
        assert_parameter_documentation(
            "\
Args:
    *args: Extra positional arguments.
    **kwargs: Extra keyword arguments.",
            &[
                ("*args", "Extra positional arguments."),
                ("**kwargs", "Extra keyword arguments."),
            ],
        );
    }

    #[test]
    fn ignores_sections_in_other_containers() {
        for raw in [
            "\
.. note::
    Args:
        nested: Not parameter documentation.",
            "\
.. note::

        Keyword Args:
            nested: Not parameter documentation.",
            "\
- Example:
    Args:
        nested: Not parameter documentation.",
            "\
- Example:

        Args:
            nested: Not parameter documentation.",
            "\
1. Example:
    Args:
        nested: Not parameter documentation.",
            "\
:param value: Example input.
    Args:
        nested: Not parameter documentation.",
            "\
Example::

        Args:
            nested: Not parameter documentation.",
        ] {
            assert_parameter_documentation(raw, &[]);
        }
    }

    #[test]
    fn resumes_after_other_containers() {
        for raw in [
            "\
Summary.

    ```text
    Args:
        nested: Not parameter documentation.
    ```

    Args:
        value: Parameter documentation.",
            "\
Summary.

    Example::

        Args:
            nested: Not parameter documentation.

    Args:
        value: Parameter documentation.",
            "\
.. note::
    Args:
        nested: Not parameter documentation.
Args:
    value: Parameter documentation.",
            "\
Example::

```
sample
```

Args:
    value: Parameter documentation.",
        ] {
            assert_parameter_documentation(raw, &[("value", "Parameter documentation.")]);
        }
    }

    #[test]
    fn backticks_in_fence_info_do_not_hide_parameter_sections() {
        assert_parameter_documentation(
            "\
```PRNGKey`` is accepted.

Args:
    value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn ignores_doctest_content_and_resumes_after_it() {
        assert_parameter_documentation(
            "        >>> example()
        Args:
            nested: Not parameter documentation.

        Args:
            value: Parameter documentation.",
            &[("value", "Parameter documentation.")],
        );
    }

    #[test]
    fn visits_structured_section_kinds_in_source_order() {
        let raw = "\
Args:
    value: Documentation.
Keyword Args:
    option: Optional.
Other Parameters:
    other: Other.
Returns:
    bool: Result.";
        let mut kinds = Vec::new();
        visit_sections(raw, |kind, _, _, _| kinds.push(kind));

        assert_eq!(
            kinds,
            [
                SectionKind::Parameters,
                SectionKind::KeywordArguments,
                SectionKind::OtherParameters,
                SectionKind::Returns,
            ]
        );
    }

    #[test]
    fn passes_section_body_range_and_header_indent_to_visitor() {
        let raw = "    Args:
        value: Documentation.
Methods:
    helper: Method documentation.";
        let mut sections = Vec::new();
        visit_sections(raw, |kind, body, range, header_indent| {
            sections.push((
                kind,
                body.iter().map(|line| line.text).collect::<Vec<_>>(),
                &raw[range],
                header_indent,
            ));
        });

        assert_eq!(
            sections,
            vec![(
                SectionKind::Parameters,
                vec!["        value: Documentation."],
                "    Args:\n        value: Documentation.",
                TextSize::new(4),
            )]
        );
    }

    fn assert_parameter_documentation(raw: &str, expected: &[(&str, &str)]) {
        let parameters = parameter_documentation(raw);
        assert_eq!(parameters.len(), expected.len(), "{raw}");

        for &(name, documentation) in expected {
            assert_eq!(
                parameters.get(name).map(String::as_str),
                Some(documentation),
                "{raw}"
            );
        }
    }
}
