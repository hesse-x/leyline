use std::fmt::{self, Display, Formatter};

pub type ProbeResult<T> = Result<T, ProbeError>;

#[derive(Debug)]
pub struct ProbeError {
    pub category: &'static str,
    pub stage: &'static str,
    pub detail: String,
    pub action: String,
    code: u8,
}

impl ProbeError {
    pub fn missing(
        stage: &'static str,
        detail: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            category: "dependency.missing",
            stage,
            detail: detail.into(),
            action: action.into(),
            code: 2,
        }
    }

    pub fn unsuitable(
        stage: &'static str,
        detail: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            category: "device.unsuitable",
            stage,
            detail: detail.into(),
            action: action.into(),
            code: 3,
        }
    }

    pub fn protocol(
        stage: &'static str,
        detail: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            category: "protocol.unsupported",
            stage,
            detail: detail.into(),
            action: action.into(),
            code: 3,
        }
    }

    pub fn internal(stage: &'static str, detail: impl Into<String>) -> Self {
        Self {
            category: "probe.internal",
            stage,
            detail: detail.into(),
            action: "rerun with RUST_BACKTRACE=1 and report the probe defect".into(),
            code: 4,
        }
    }

    pub const fn exit_code(&self) -> u8 {
        self.code
    }
}

impl Display for ProbeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "category={} stage={} result=failed detail={} action={}",
            self.category,
            self.stage,
            escape(&self.detail),
            escape(&self.action)
        )
    }
}

impl std::error::Error for ProbeError {}

pub struct Reporter {
    json: bool,
    verbose: bool,
    records: Vec<(&'static str, &'static str, String)>,
}

impl Reporter {
    pub const fn new(json: bool, verbose: bool) -> Self {
        Self {
            json,
            verbose,
            records: Vec::new(),
        }
    }

    pub fn pass(&mut self, probe: &'static str, stage: &'static str, detail: impl Into<String>) {
        let detail = detail.into();
        if !self.json {
            println!(
                "probe={probe} stage={stage} result=passed detail={}",
                escape(&detail)
            );
        }
        self.records.push((probe, stage, detail));
    }

    pub fn note(&self, probe: &str, stage: &str, detail: impl AsRef<str>) {
        if self.verbose && !self.json {
            println!(
                "probe={probe} stage={stage} result=info detail={}",
                escape(detail.as_ref())
            );
        }
    }

    pub fn finish(&self) {
        if self.json {
            print!("{{\"result\":\"passed\",\"records\":[");
            for (index, (probe, stage, detail)) in self.records.iter().enumerate() {
                if index > 0 {
                    print!(",");
                }
                print!(
                    "{{\"probe\":\"{}\",\"stage\":\"{}\",\"detail\":\"{}\"}}",
                    json(probe),
                    json(stage),
                    json(detail)
                );
            }
            println!("]}}");
        }
    }

    pub fn failure(&self, error: &ProbeError) {
        if self.json {
            println!(
                "{{\"result\":\"failed\",\"category\":\"{}\",\"stage\":\"{}\",\"detail\":\"{}\",\"action\":\"{}\"}}",
                error.category,
                error.stage,
                json(&error.detail),
                json(&error.action)
            );
        } else {
            eprintln!("{error}");
        }
    }
}

fn escape(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}
fn json(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\u{08}' => escaped.push_str("\\b"),
            '\u{0c}' => escaped.push_str("\\f"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            value if value <= '\u{1f}' => {
                use std::fmt::Write;
                let _ = write!(escaped, "\\u{:04x}", value as u32);
            }
            value => escaped.push(value),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::json;

    #[test]
    fn json_escape_preserves_unicode_and_apostrophes() {
        assert_eq!(json("中'a\n\"\\"), "中'a\\n\\\"\\\\");
    }
}
