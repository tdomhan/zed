pub(crate) fn before_input_text<'a>(
    composing: bool,
    input_type: &str,
    data: Option<&'a str>,
    cancelable: bool,
) -> Option<&'a str> {
    // Non-cancellable edits belong to input, after the browser changes the value.
    if !cancelable
        || composing
        || !input_type.starts_with("insert")
        || input_type.contains("Composition")
    {
        return None;
    }
    data.filter(|text| !text.is_empty())
}

pub(crate) fn input_text<'a>(composing: bool, input_type: &str, value: &'a str) -> Option<&'a str> {
    if composing || input_type.contains("Composition") || value.is_empty() {
        None
    } else {
        Some(value)
    }
}
