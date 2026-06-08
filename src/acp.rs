use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpPhase {
    AwaitingAssistant,
    AwaitingAssistantAfterTools,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpViolation {
    EmptyAssistantTurn,
    EmptyAssistantAfterToolResults,
    RepeatedRecoverableToolFailure,
    ParserStreamError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpAssessment {
    pub phase: AcpPhase,
    pub backend: String,
    pub status: String,
    pub violation: Option<AcpViolation>,
    pub repair_prompt: Option<String>,
    pub note: String,
}

impl AcpAssessment {
    pub fn ok(backend: &str, phase: AcpPhase, note: &str) -> Self {
        Self {
            phase,
            backend: backend.to_string(),
            status: "ok".to_string(),
            violation: None,
            repair_prompt: None,
            note: note.to_string(),
        }
    }

    pub fn wdf(
        backend: &str,
        phase: AcpPhase,
        violation: AcpViolation,
        note: &str,
        repair_prompt: String,
    ) -> Self {
        Self {
            phase,
            backend: backend.to_string(),
            status: "wdf_repair".to_string(),
            violation: Some(violation),
            repair_prompt: Some(repair_prompt),
            note: note.to_string(),
        }
    }
}

pub fn assess_assistant_turn(
    backend: &str,
    phase: AcpPhase,
    text: &str,
    tool_call_count: usize,
    tool_summary: Option<&str>,
) -> AcpAssessment {
    if tool_call_count > 0 {
        return AcpAssessment::ok(backend, phase, "assistant produced tool call(s)");
    }
    if !text.trim().is_empty() {
        return AcpAssessment::ok(backend, phase, "assistant produced text");
    }

    match phase {
        AcpPhase::AwaitingAssistantAfterTools => {
            let summary = tool_summary.unwrap_or("No tool checkpoint summary is available.");
            AcpAssessment::wdf(
                backend,
                phase,
                AcpViolation::EmptyAssistantAfterToolResults,
                "assistant returned empty output after tool results",
                wdf_tool_continuation_prompt(summary),
            )
        }
        AcpPhase::AwaitingAssistant => AcpAssessment::wdf(
            backend,
            phase,
            AcpViolation::EmptyAssistantTurn,
            "assistant returned empty output before any tool result checkpoint",
            wdf_empty_assistant_prompt(),
        ),
    }
}

pub fn wdf_tool_continuation_prompt(summary: &str) -> String {
    format!(
        "ACP/WDF recovery: Wire just delivered tool results and the provider returned no assistant text or next tool call. Continue the exact same task from the checkpoint below. Do not restart, do not repeat completed tool calls, and do not stop just because a tool failed. Produce either final text, an updated plan, or the next necessary tool call.\n\n{summary}"
    )
}

pub fn wdf_empty_assistant_prompt() -> String {
    "ACP/WDF recovery: the provider returned an empty assistant turn. Continue the active task. Produce final text, a concise plan, or the next necessary tool call. Do not emit an empty response again.".to_string()
}

pub fn wdf_parser_error_prompt(backend: &str, error: &str) -> String {
    format!(
        "ACP/WDF recovery: Wire stopped parsing the {backend} stream after a protocol/parser error to avoid wasting tokens. Continue the same task with a valid assistant turn. Do not repeat malformed tool calls or malformed JSON. Parser error summary: {error}"
    )
}

#[cfg(test)]
mod tests {
    use super::{assess_assistant_turn, AcpPhase, AcpViolation};

    #[test]
    fn empty_after_tools_requests_wdf_repair() {
        let assessment = assess_assistant_turn(
            "chat_completions",
            AcpPhase::AwaitingAssistantAfterTools,
            "",
            0,
            Some("tool summary"),
        );

        assert_eq!(
            assessment.violation,
            Some(AcpViolation::EmptyAssistantAfterToolResults)
        );
        assert!(assessment
            .repair_prompt
            .unwrap()
            .contains("ACP/WDF recovery"));
    }

    #[test]
    fn text_or_tool_call_is_valid() {
        assert!(
            assess_assistant_turn("responses", AcpPhase::AwaitingAssistant, "done", 0, None,)
                .violation
                .is_none()
        );
        assert!(assess_assistant_turn(
            "responses",
            AcpPhase::AwaitingAssistantAfterTools,
            "",
            1,
            None,
        )
        .violation
        .is_none());
    }
}
