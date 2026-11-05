//! The response vocabulary: the head as it arrived, and the body that followed.
//!
//! Fields are private and read through accessors, so a test states what it expects of the status, a
//! header or the body instead of rebuilding a response. [`Head::parse`] is the one place the status
//! line and headers are decoded.

/// A response head: status and headers, in the order they arrived.
#[derive(Debug, Clone)]
pub(crate) struct Head {
    status: u16,
    headers: Vec<(String, String)>,
}

impl Head {
    /// Returns the status code.
    pub(crate) fn status(&self) -> u16 {
        self.status
    }

    /// Returns the first value of `name`, case-insensitively.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(existing, _value)| existing.eq_ignore_ascii_case(name))
            .map(|(_name, value)| value.as_str())
    }

    /// Returns every `(name, value)` pair the peer sent, names lowercased.
    pub(crate) fn headers(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Returns whether the body is chunked rather than length-framed.
    pub(crate) fn is_chunked(&self) -> bool {
        self.header("transfer-encoding")
            .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    }

    /// Returns the declared body length, when the response declares one.
    pub(crate) fn content_length(&self) -> Option<usize> {
        self.header("content-length")
            .and_then(|value| value.parse::<usize>().ok())
    }

    /// Returns whether the response cannot carry a body at all.
    pub(crate) fn body_forbidden(&self) -> bool {
        (100..200).contains(&self.status) || self.status == 204 || self.status == 304
    }

    /// Parses a status line and the headers that follow it.
    ///
    /// The reason phrase is not part of any assertion, so it is not kept.
    pub(crate) fn parse(raw: &[u8]) -> Self {
        let text = String::from_utf8_lossy(raw);
        let mut lines = text.split("\r\n");
        let status_line = lines.next().unwrap_or_default();
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or_default();
        let status = parts
            .next()
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or_else(|| panic!("the status line is not HTTP: {status_line:?}"));
        assert!(
            version.starts_with("HTTP/1."),
            "the gateway answered with {status_line:?}, which is not HTTP/1.x"
        );

        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                panic!("the response contains a header without a colon: {line:?}");
            };
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }

        Self { status, headers }
    }
}

/// A complete response: head and body.
#[derive(Debug, Clone)]
pub(crate) struct Response {
    head: Head,
    body: Vec<u8>,
}

impl Response {
    /// Builds a response from a head and the bytes that followed it.
    pub(crate) fn new(head: Head, body: Vec<u8>) -> Self {
        Self { head, body }
    }

    /// Returns the status code.
    pub(crate) fn status(&self) -> u16 {
        self.head.status()
    }

    /// Returns the value of response header `name`, when it was present.
    ///
    /// The scenarios that assert a rejection's headers read it from here rather than driving the
    /// connection by hand: the headers are the subject, not the framing.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.head.header(name)
    }

    /// Returns the body bytes.
    pub(crate) fn body(&self) -> &[u8] {
        &self.body
    }

    /// Returns the body as lossy UTF-8, for assertion messages.
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Returns the stable error `code` the data plane puts in a JSON error body.
    pub(crate) fn json_code(&self) -> Option<String> {
        json_string(&self.text(), "code")
    }

    /// Returns the `trace_id` the data plane echoes into a JSON error body.
    pub(crate) fn json_trace_id(&self) -> Option<String> {
        json_string(&self.text(), "trace_id")
    }
}

/// Reads the first `"<field>":"<value>"` occurrence from a flat JSON object.
///
/// The bodies under test are written by this workspace, contain no nesting for these fields and
/// only escape a trace id, so a scan is enough; adding a JSON parser to a test helper would be a
/// second thing to keep correct.
fn json_string(text: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let mut value = String::new();
    let mut escaped = false;
    for character in rest.chars() {
        if escaped {
            value.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => return Some(value),
            character => value.push(character),
        }
    }
    None
}
