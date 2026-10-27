use serde_json::Value;
#[cfg(test)]
use serde_json::json;

#[derive(Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct TokenCalibration {
    route: String,
    ratio: Option<f64>,
}

impl TokenCalibration {
    pub fn estimate(&self, value: &Value, route: &str) -> usize {
        let ratio = if self.route == route {
            self.ratio.unwrap_or(1.0)
        } else {
            1.0
        };
        (estimate(value) as f64 * ratio).ceil() as usize
    }

    pub fn observe(&mut self, route: &str, estimated: usize, actual: usize) {
        if estimated == 0 || actual == 0 {
            return;
        }
        self.route = route.to_owned();
        // Include modest headroom for tokenizer/framing differences. Missing or
        // implausible usage must not make the host's estimate unbounded.
        self.ratio = Some((actual as f64 / estimated as f64 * 1.05).clamp(0.1, 10.0));
    }
}

/// Maximum field of view, not a target request size. Providers do not share a
/// tokenizer; use a conservative text estimate and reserve room for output.
pub const MAX_TOKENS: usize = 256_000;
pub fn estimate(value: &Value) -> usize {
    match value {
        Value::String(text) => {
            let ascii = text.bytes().filter(u8::is_ascii).count();
            let unicode = text.chars().filter(|ch| !ch.is_ascii()).count();
            ascii.div_ceil(3) + unicode * 2
        }
        Value::Array(items) => items.iter().map(estimate).sum::<usize>() + items.len() * 4,
        // Image transport bytes are not text tokens. This is an allowance for
        // the supported task image input, not a count of its base64 encoding.
        Value::Object(fields)
            if matches!(
                fields.get("type").and_then(Value::as_str),
                Some("image_url" | "image_ref")
            ) =>
        {
            4096
        }
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| key.len().div_ceil(3) + estimate(value) + 4)
            .sum(),
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calibration_is_scoped_to_provider_model_and_ignores_missing_usage() {
        let mut calibration = TokenCalibration::default();
        let input = json!({"messages":[{"role":"user","content":"中文材料".repeat(500)}]});
        let raw = estimate(&input);
        assert_eq!(calibration.estimate(&input, "a/model"), raw);
        calibration.observe("a/model", raw, raw / 3);
        let adjusted = calibration.estimate(&input, "a/model");
        assert!(adjusted < raw / 2 && adjusted >= raw / 3);
        assert_eq!(calibration.estimate(&input, "b/model"), raw);
        calibration.observe("a/model", raw, 0);
        assert_eq!(calibration.estimate(&input, "a/model"), adjusted);
        let restored: TokenCalibration =
            serde_json::from_value(serde_json::to_value(&calibration).unwrap()).unwrap();
        assert_eq!(restored.estimate(&input, "a/model"), adjusted);
    }
    #[test]
    fn image_references_and_wire_images_have_same_budget() {
        assert_eq!(
            estimate(&json!({"type":"image_ref","artifact_id":"v"})),
            estimate(&json!({"type":"image_url","image_url":{"url":"data:image/png;base64,abc"}}))
        );
    }
}
