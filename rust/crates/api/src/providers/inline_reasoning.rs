//! Azure DeepSeek deployments can return leading `<think>` content instead of
//! `reasoning_content`. Keep that protocol envelope out of the final answer.

use crate::error::ApiError;

#[derive(Debug)]
enum Phase {
    Prefix,
    Thinking,
    Answer,
}

#[derive(Debug)]
pub(super) struct InlineReasoning {
    phase: Phase,
    pending: String,
}

impl InlineReasoning {
    pub(super) fn for_model(model: &str) -> Option<Self> {
        let model = model.to_ascii_lowercase();
        let model = model.rsplit('/').next().unwrap_or(&model);
        (model.starts_with("deepseek-") && model.ends_with("-azure")).then(|| Self {
            phase: Phase::Prefix,
            pending: String::new(),
        })
    }

    /// Each pair contains whether the segment is reasoning and its text.
    pub(super) fn push(&mut self, text: &str) -> Vec<(bool, String)> {
        self.pending.push_str(text);
        let mut segments = Vec::new();
        if matches!(self.phase, Phase::Prefix) {
            let prefix = self.pending.trim_start();
            if let Some(rest) = prefix.strip_prefix("<think>") {
                self.pending = rest.to_owned();
                self.phase = Phase::Thinking;
            } else if "<think>".starts_with(prefix) {
                return segments;
            } else {
                self.phase = Phase::Answer;
            }
        }
        if matches!(self.phase, Phase::Thinking) {
            const CLOSE: &str = "</think>";
            if let Some(end) = self.pending.find(CLOSE) {
                if end > 0 {
                    segments.push((true, self.pending[..end].to_owned()));
                }
                self.pending.drain(..end + CLOSE.len());
                self.phase = Phase::Answer;
            } else {
                // Retain only a possible split closing tag. The matching ASCII
                // suffix also establishes a UTF-8 boundary for this byte split.
                let retained = (1..CLOSE.len())
                    .rev()
                    .find(|&len| self.pending.ends_with(&CLOSE[..len]))
                    .unwrap_or(0);
                let end = self.pending.len() - retained;
                if end > 0 {
                    segments.push((true, self.pending[..end].to_owned()));
                    self.pending.drain(..end);
                }
            }
        }
        if matches!(self.phase, Phase::Answer) && !self.pending.is_empty() {
            segments.push((false, std::mem::take(&mut self.pending)));
        }
        segments
    }

    pub(super) fn finish(&mut self) -> Result<Vec<(bool, String)>, ApiError> {
        if matches!(self.phase, Phase::Thinking) {
            return Err(ApiError::InvalidSseFrame(
                "Azure DeepSeek inline reasoning is missing its closing tag",
            ));
        }
        Ok(if self.pending.is_empty() {
            Vec::new()
        } else {
            vec![(false, std::mem::take(&mut self.pending))]
        })
    }
}
