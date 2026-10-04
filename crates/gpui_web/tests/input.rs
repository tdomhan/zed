// Compile the production input policy natively so these tests execute without a browser.
#[path = "../src/input.rs"]
mod input;

use input::{before_input_text, input_text};

#[test]
fn browser_insertions_have_one_owner() {
    for (has_before_input, cancelable) in [(true, true), (true, false), (false, false)] {
        for input_type in ["insertText", "insertReplacementText"] {
            let text = "hello 🙂";
            let before = has_before_input
                .then(|| before_input_text(false, input_type, Some(text), cancelable))
                .flatten();
            let browser_value = if before.is_some() { "" } else { text };
            let after = input_text(false, input_type, browser_value);
            assert_eq!(
                before.into_iter().chain(after).collect::<Vec<_>>(),
                vec![text]
            );
        }
    }
}

#[test]
fn composition_keeps_ownership_of_preedit_and_commit() {
    for input_type in [
        "insertText",
        "insertCompositionText",
        "insertFromComposition",
    ] {
        assert_eq!(
            before_input_text(true, input_type, Some("日本"), true),
            None
        );
        assert_eq!(input_text(true, input_type, "日本"), None);
    }
    for input_type in ["insertCompositionText", "insertFromComposition"] {
        assert_eq!(
            before_input_text(false, input_type, Some("日本"), true),
            None
        );
        assert_eq!(input_text(false, input_type, "日本"), None);
    }
}

#[test]
fn empty_and_non_insertion_before_input_are_ignored() {
    assert_eq!(before_input_text(false, "insertText", None, true), None);
    assert_eq!(before_input_text(false, "insertText", Some(""), true), None);
    assert_eq!(
        before_input_text(false, "deleteContentBackward", Some("x"), true),
        None
    );
    assert_eq!(input_text(false, "insertText", ""), None);
}
