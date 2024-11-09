use http::{HeaderValue, StatusCode};

#[derive(Clone)]
pub struct Redirect {
    pub path: String,
    pub target: HeaderValue,
    pub code: StatusCode,
}

pub fn parse(redirect_file: &str) -> Result<Vec<Redirect>, RedirectParseError> {
    let mut redirects = Vec::new();
    for (idx, line) in redirect_file.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            // handle comments
            continue;
        }

        let items = line.split_ascii_whitespace().collect::<Vec<&str>>();
        if !(2..=3).contains(&items.len()) {
            return Err(RedirectParseError::new(
                RedirectParseErrorKind::WrongOptCount(items.len()),
                idx,
            ));
        }

        let path = items[0].to_string();
        let Ok(target) = HeaderValue::from_str(items[1]) else {
            return Err(RedirectParseError::new(
                RedirectParseErrorKind::HeaderValue(items[1].to_string()),
                idx,
            ));
        };

        let code: StatusCode = if let Some(code_str) = items.get(2) {
            let Ok(code) = code_str.parse() else {
                return Err(RedirectParseError::new(
                    RedirectParseErrorKind::StatusCode(code_str.to_string()),
                    idx,
                ));
            };
            code
        } else {
            StatusCode::TEMPORARY_REDIRECT
        };
        redirects.push(Redirect { path, target, code });
    }
    Ok(redirects)
}

#[derive(Debug, thiserror::Error)]
#[error("{kind}")]
pub struct RedirectParseError {
    row: usize,
    #[source]
    kind: RedirectParseErrorKind,
}

impl RedirectParseError {
    fn new(kind: RedirectParseErrorKind, idx: usize) -> Self {
        Self { row: idx + 1, kind }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RedirectParseErrorKind {
    #[error("Wrong number of entries on a line: {0}, expected 2 or 3")]
    WrongOptCount(usize),
    #[error("`{0}` is an invalid header value")]
    HeaderValue(String),
    #[error("`{0}` could not be converted to a status")]
    StatusCode(String),
}
