use super::*;

#[test]
fn escapes_applescript_string_literals_safely() {
    assert_eq!(
        escape_applescript_literal("a \"quote\" \\ slash\nline2"),
        "a \\\"quote\\\" \\\\ slash\\nline2"
    );
}

#[test]
fn parses_osascript_button_outcomes() {
    assert_eq!(
        parse_osascript_outcome("button returned:Approve, gave up:false\n"),
        ApprovalPromptOutcome::Approve
    );
    assert_eq!(
        parse_osascript_outcome("button returned:Reject, gave up:false\n"),
        ApprovalPromptOutcome::Reject
    );
    assert_eq!(
        parse_osascript_outcome("button returned:Later, gave up:false\n"),
        ApprovalPromptOutcome::Pending
    );
    assert_eq!(
        parse_osascript_outcome("button returned:, gave up:true\n"),
        ApprovalPromptOutcome::Pending
    );
}
