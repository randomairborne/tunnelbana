use http::{
    header::{InvalidHeaderName, InvalidHeaderValue},
    HeaderName, HeaderValue,
};

#[derive(Clone)]
pub struct HeaderGroup {
    pub path: String,
    pub targets: Vec<(HeaderName, HeaderValue)>,
}

pub fn parse(header_file: &str) -> Result<Vec<HeaderGroup>, HeaderParseError> {
    let mut headers = Vec::new();
    let mut current_ctx: Option<HeaderGroup> = None;
    for (idx, line) in header_file.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            // handle comments
            continue;
        }
        if line.starts_with(['\t', ' ']) {
            let Some(ctx) = current_ctx.as_mut() else {
                return Err(HeaderParseError::new(HeaderParseErrorKind::NoParseCtx, idx));
            };
            let (name, value) = line
                .trim()
                .split_once(':')
                .ok_or_else(|| HeaderParseError::new(HeaderParseErrorKind::NoHeaderColon, idx))?;
            let name = match HeaderName::from_bytes(name.as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    return Err(HeaderParseError::new(
                        HeaderParseErrorKind::HeaderNameParse(e),
                        idx,
                    ))
                }
            };
            let value = match HeaderValue::from_bytes(value.as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    return Err(HeaderParseError::new(
                        HeaderParseErrorKind::HeaderValueParse(e),
                        idx,
                    ))
                }
            };

            ctx.targets.push((name, value));
        } else {
            let mut group = Some(HeaderGroup {
                path: line.trim().to_string(),
                targets: Vec::new(),
            });
            std::mem::swap(&mut current_ctx, &mut group);
            if let Some(group) = group {
                headers.push(group);
            }
        }
    }
    Ok(headers)
}

#[derive(Debug, thiserror::Error)]
#[error("at line {row}: {kind}")]
pub struct HeaderParseError {
    row: usize,
    #[source]
    kind: HeaderParseErrorKind,
}

impl HeaderParseError {
    fn new(kind: HeaderParseErrorKind, idx: usize) -> Self {
        Self { row: idx + 1, kind }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HeaderParseErrorKind {
    #[error("Header name invalid: {0}")]
    HeaderNameParse(#[from] InvalidHeaderName),
    #[error("Header name value: {0}")]
    HeaderValueParse(#[from] InvalidHeaderValue),
    #[error("You must specify an unindented path before specifying headers")]
    NoParseCtx,
    #[error("You must put a colon at the end of the header name")]
    NoHeaderColon,
}
