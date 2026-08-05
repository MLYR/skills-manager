use serde_json::Value;

use super::command_error;
use super::types::{AiAnalysisResultV1, AiCommandError, AiErrorCode, AiErrorKind};

pub const MAX_MODEL_RESPONSE_BYTES: usize = 1_048_576;
pub const MAX_RESULT_JSON_BYTES: usize = 65_536;
const MAX_ONE_LINE_CHARS: usize = 60;
const MAX_WHAT_IT_DOES_CHARS: usize = 4_000;
const MAX_ARRAY_ITEM_CHARS: usize = 1_000;
const MAX_STANDARD_ARRAY_ITEMS: usize = 20;
const MAX_EXAMPLE_PROMPTS: usize = 10;
pub const UNSPECIFIED_PLACEHOLDER: &str = "原文未说明";

pub fn validate_ai_analysis_result_v1(
    raw_response: &[u8],
) -> Result<AiAnalysisResultV1, AiCommandError> {
    if raw_response.len() > MAX_MODEL_RESPONSE_BYTES {
        return Err(schema_validation_error());
    }

    let value: Value = serde_json::from_slice(raw_response).map_err(|_| invalid_json_error())?;
    if !value.is_object() {
        return Err(schema_validation_error());
    }

    // AiAnalysisResultV1 denies unknown fields, so deserialization enforces the
    // frozen v1 field set before any normalized result can reach persistence.
    let mut result: AiAnalysisResultV1 =
        serde_json::from_value(value).map_err(|_| schema_validation_error())?;
    result.one_line = normalize_string(&result.one_line, "one_line", MAX_ONE_LINE_CHARS)?;
    result.what_it_does =
        normalize_string(&result.what_it_does, "what_it_does", MAX_WHAT_IT_DOES_CHARS)?;
    result.when_to_use =
        normalize_list(&result.when_to_use, "when_to_use", MAX_STANDARD_ARRAY_ITEMS)?;
    result.how_to_use = normalize_list(&result.how_to_use, "how_to_use", MAX_STANDARD_ARRAY_ITEMS)?;
    result.example_prompts = normalize_list(
        &result.example_prompts,
        "example_prompts",
        MAX_EXAMPLE_PROMPTS,
    )?;
    result.requirements = normalize_list(
        &result.requirements,
        "requirements",
        MAX_STANDARD_ARRAY_ITEMS,
    )?;
    result.not_for = normalize_list(&result.not_for, "not_for", MAX_STANDARD_ARRAY_ITEMS)?;
    result.warnings = normalize_list(&result.warnings, "warnings", MAX_STANDARD_ARRAY_ITEMS)?;

    // Serialize the normalized result now, rather than trusting a later caller
    // to notice that a valid-looking response cannot fit the persisted envelope.
    let compact_result = serde_json::to_vec(&result).map_err(|_| schema_validation_error())?;
    if compact_result.len() > MAX_RESULT_JSON_BYTES {
        return Err(schema_validation_error());
    }

    Ok(result)
}

fn normalize_list(
    values: &[String],
    field: &str,
    maximum_items: usize,
) -> Result<Vec<String>, AiCommandError> {
    if values.len() > maximum_items {
        return Err(schema_limit_error(field, maximum_items, "items"));
    }

    values
        .iter()
        .map(|value| normalize_string(value, field, MAX_ARRAY_ITEM_CHARS))
        .collect()
}

fn normalize_string(
    value: &str,
    field: &str,
    maximum_chars: usize,
) -> Result<String, AiCommandError> {
    if value.contains('\0') {
        return Err(schema_field_error(field));
    }

    let normalized = value.trim();
    if normalized == UNSPECIFIED_PLACEHOLDER {
        // 占位词无法帮助用户理解 Skill；应由模型基于上下文归纳，或让数组为空。
        return Err(schema_field_error(field));
    }
    let character_count = normalized.chars().count();
    if character_count == 0 || character_count > maximum_chars {
        return Err(schema_limit_error(field, maximum_chars, "characters"));
    }

    Ok(normalized.to_owned())
}

fn invalid_json_error() -> AiCommandError {
    command_error(
        AiErrorKind::Provider,
        AiErrorCode::InvalidJson,
        "The AI provider returned invalid JSON.",
        false,
    )
}

fn schema_validation_error() -> AiCommandError {
    command_error(
        AiErrorKind::Provider,
        AiErrorCode::SchemaValidation,
        "The AI provider response does not match analysis schema v1.",
        false,
    )
}

fn schema_field_error(field: &str) -> AiCommandError {
    command_error(
        AiErrorKind::Provider,
        AiErrorCode::SchemaValidation,
        format!("The AI provider response violates analysis schema v1 at {field}."),
        false,
    )
}

fn schema_limit_error(field: &str, maximum: usize, unit: &str) -> AiCommandError {
    command_error(
        AiErrorKind::Provider,
        AiErrorCode::SchemaValidation,
        format!(
            "The AI provider response violates analysis schema v1: {field} exceeds {maximum} {unit}."
        ),
        false,
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn valid_response() -> Value {
        json!({
            "one_line": "Summarizes a Skill.",
            "what_it_does": "Explains what the Skill does for a user.",
            "when_to_use": ["When evaluating a Skill."],
            "how_to_use": ["Read the generated summary."],
            "example_prompts": ["Explain this Skill."],
            "requirements": [],
            "not_for": [],
            "warnings": ["Check the original document for details."]
        })
    }

    fn validate(value: Value) -> Result<AiAnalysisResultV1, AiCommandError> {
        validate_ai_analysis_result_v1(value.to_string().as_bytes())
    }

    #[test]
    fn accepts_and_normalizes_a_complete_valid_result() {
        let mut response = valid_response();
        response["one_line"] = json!("  Summarizes a Skill.  ");
        response["when_to_use"] = json!(["  When evaluating a Skill.  "]);

        let result = validate(response).expect("valid response should pass");
        assert_eq!(result.one_line, "Summarizes a Skill.");
        assert_eq!(result.when_to_use, ["When evaluating a Skill."]);
    }

    #[test]
    fn rejects_invalid_json_without_echoing_provider_content() {
        let secret_text = b"{invalid-json-secret-tail";
        let error =
            validate_ai_analysis_result_v1(secret_text).expect_err("invalid JSON must fail");

        assert_eq!(error.code, AiErrorCode::InvalidJson);
        assert!(!error.message.contains("secret-tail"));
    }

    #[test]
    fn rejects_missing_unknown_and_wrong_type_fields() {
        let mut missing = valid_response();
        missing.as_object_mut().expect("object").remove("warnings");
        assert_eq!(
            validate(missing).expect_err("missing field must fail").code,
            AiErrorCode::SchemaValidation
        );

        let mut unknown = valid_response();
        unknown["untrusted_extra"] = json!("secret-tail");
        let error = validate(unknown).expect_err("unknown field must fail");
        assert_eq!(error.code, AiErrorCode::SchemaValidation);
        assert!(!error.message.contains("secret-tail"));

        let mut wrong_type = valid_response();
        wrong_type["when_to_use"] = json!([42]);
        assert_eq!(
            validate(wrong_type)
                .expect_err("non-string array must fail")
                .code,
            AiErrorCode::SchemaValidation
        );
    }

    #[test]
    fn rejects_empty_nul_and_out_of_bounds_values() {
        let mut empty = valid_response();
        empty["one_line"] = json!("   ");
        assert_eq!(
            validate(empty).expect_err("blank string must fail").code,
            AiErrorCode::SchemaValidation
        );

        let mut placeholder = valid_response();
        placeholder["how_to_use"] = json!([UNSPECIFIED_PLACEHOLDER]);
        let error = validate(placeholder).expect_err("placeholder must fail");
        assert_eq!(error.code, AiErrorCode::SchemaValidation);
        assert!(error.message.contains("how_to_use"));

        let mut nul = valid_response();
        nul["what_it_does"] = json!("safe\u{0}tail");
        assert_eq!(
            validate(nul).expect_err("NUL must fail").code,
            AiErrorCode::SchemaValidation
        );

        let mut one_line = valid_response();
        one_line["one_line"] = json!("a".repeat(MAX_ONE_LINE_CHARS + 1));
        let error = validate(one_line).expect_err("one_line limit must fail");
        assert_eq!(error.code, AiErrorCode::SchemaValidation);
        assert!(error.message.contains("one_line exceeds 60 characters"));

        let mut examples = valid_response();
        examples["example_prompts"] = json!(vec!["prompt"; MAX_EXAMPLE_PROMPTS + 1]);
        let error = validate(examples).expect_err("example array limit must fail");
        assert_eq!(error.code, AiErrorCode::SchemaValidation);
        assert!(error.message.contains("example_prompts exceeds 10 items"));

        let mut standard_list = valid_response();
        standard_list["requirements"] = json!(vec!["requirement"; MAX_STANDARD_ARRAY_ITEMS + 1]);
        assert_eq!(
            validate(standard_list)
                .expect_err("standard array limit must fail")
                .code,
            AiErrorCode::SchemaValidation
        );

        let mut item = valid_response();
        item["warnings"] = json!(["a".repeat(MAX_ARRAY_ITEM_CHARS + 1)]);
        assert_eq!(
            validate(item).expect_err("array item limit must fail").code,
            AiErrorCode::SchemaValidation
        );
    }

    #[test]
    fn rejects_overlarge_raw_and_compact_results() {
        assert_eq!(
            validate_ai_analysis_result_v1(&vec![b' '; MAX_MODEL_RESPONSE_BYTES + 1])
                .expect_err("raw response limit must fail")
                .code,
            AiErrorCode::SchemaValidation
        );

        let mut result = valid_response();
        result["how_to_use"] = json!(vec!["a".repeat(MAX_ARRAY_ITEM_CHARS); 20]);
        result["requirements"] = json!(vec!["b".repeat(MAX_ARRAY_ITEM_CHARS); 20]);
        result["not_for"] = json!(vec!["c".repeat(MAX_ARRAY_ITEM_CHARS); 20]);
        result["warnings"] = json!(vec!["d".repeat(MAX_ARRAY_ITEM_CHARS); 20]);
        assert_eq!(
            validate(result)
                .expect_err("compact persisted result limit must fail")
                .code,
            AiErrorCode::SchemaValidation
        );
    }
}
