use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind as IoErrorKind,
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};

use http::HeaderValue;

#[derive(Debug)]
#[allow(clippy::module_name_repetitions)]
/// A map of String-based static paths to etag sets.
/// This serves as a simple wrapper type, just to prove that the created
/// map is valid.
pub struct ETagMap {
    map: HashMap<String, Arc<ResourceTagSet>>,
}

impl Deref for ETagMap {
    type Target = HashMap<String, Arc<ResourceTagSet>>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

/// A set of resource tags and their invertible set.
/// This is used to implement both returning the correct
/// `ETag` based on content encoding, and responding with 304
/// if any compression permutation at that location matches a
/// stored resource tag.
#[derive(Debug, Clone)]
pub struct ResourceTagSet {
    tags: ResourceTags,
    contained_tags: HashSet<HeaderValue>,
}

impl ResourceTagSet {
    /// Find if this set has a header, to decide if we want to return
    /// a 304
    pub fn contains_tag(&self, value: &HeaderValue) -> bool {
        self.contained_tags.contains(value)
    }
}

#[derive(Debug, Clone)]
/// List of resource tags, matched to their compression permutation.
/// Compressed tags are optional. If they are not present, it is assumed
/// that the compression will not be sent- and no etag is returned if a wrong
/// etag is sent.
pub struct ResourceTags {
    pub raw: HeaderValue,
    pub gzip: Option<HeaderValue>,
    pub zstd: Option<HeaderValue>,
    pub deflate: Option<HeaderValue>,
    pub brotli: Option<HeaderValue>,
}

impl Deref for ResourceTagSet {
    type Target = ResourceTags;

    fn deref(&self) -> &Self::Target {
        &self.tags
    }
}

impl From<ResourceTags> for ResourceTagSet {
    fn from(value: ResourceTags) -> Self {
        Self {
            tags: value.clone(),
            contained_tags: value.setify(),
        }
    }
}

impl ResourceTags {
    /// Make a [`HashSet`] of all the [`Some`] and non-optional tags.
    fn setify(self) -> HashSet<HeaderValue> {
        fn insert_if_some(s: &mut HashSet<HeaderValue>, v: Option<HeaderValue>) {
            if let Some(v) = v {
                s.insert(v);
            }
        }
        let mut output = HashSet::with_capacity(5);
        output.insert(self.raw);
        insert_if_some(&mut output, self.gzip);
        insert_if_some(&mut output, self.zstd);
        insert_if_some(&mut output, self.brotli);
        insert_if_some(&mut output, self.deflate);
        output
    }
}

impl ETagMap {
    /// Create a new [`ETagMap`] for all the files in this directory.
    /// `.gz`, `.zz`, `.zst`, and `.br` files will be automatically incorporated
    /// into their parents.
    /// # Errors
    /// This function can error if mmap fails in blake3, or if paths cannot be generated
    pub fn new(base_dir: &Path) -> Result<Self, TagMapBuildError> {
        let files = get_file_list(base_dir)?;
        trace!(?files, count = files.len(), "Hashing files");

        let mut map = HashMap::new();
        for path in files {
            let relative_path = path
                .strip_prefix(base_dir)?
                .to_str()
                .ok_or(TagMapBuildError::PathNotStr)?;
            let key = format!("/{relative_path}");

            let tags = get_resource_tags(&path)?;

            map.insert(key, Arc::new(tags.into()));
        }
        info!(count = map.len(), "Hashed files");
        Ok(Self { map })
    }
}

fn get_resource_tags(path: &Path) -> Result<ResourceTags, TagMapBuildError> {
    Ok(ResourceTags {
        raw: file_header_hash(path, "")?,
        gzip: file_header_hash_opt(path, ".gz")?,
        zstd: file_header_hash_opt(path, ".zst")?,
        deflate: file_header_hash_opt(path, ".zz")?,
        brotli: file_header_hash_opt(path, ".br")?,
    })
}

fn file_header_hash_opt(path: &Path, ext: &str) -> Result<Option<HeaderValue>, TagMapBuildError> {
    // we try to hash all the supported extensions here- so we don't really know if each file has those
    match file_header_hash(path, ext) {
        Err(TagMapBuildError::Io(ie)) if matches!(ie.kind(), IoErrorKind::NotFound) => Ok(None),
        v => v.map(Some),
    }
}

fn file_header_hash(path: &Path, ext: &str) -> Result<HeaderValue, TagMapBuildError> {
    // Create a pathbuf and push a new textual extension to it
    let mut path = path.to_path_buf();
    path.as_mut_os_string().push(ext);
    // This is basically just `b3sum` but rust
    trace!(?path, "Hashing file");
    let hash = blake3::Hasher::new().update_mmap_rayon(&path)?.finalize();

    let hash = hash.to_hex();
    let value = HeaderValue::from_str(&format!("\"{hash}\""))?;

    Ok(value)
}

fn get_file_list(path: &Path) -> Result<Vec<PathBuf>, TagMapBuildError> {
    trace!(?path, "Reading directory");
    let dir = std::fs::read_dir(path)?;
    let mut paths = Vec::new();
    for file in dir {
        let file = file?;
        let kind = file.file_type()?;
        let path = file.path();
        if kind.is_dir() {
            let mut dir = get_file_list(&path)?;
            paths.append(&mut dir);
        } else if kind.is_file() {
            trace!(?path, "Found file");
            paths.push(path);
        } else {
            return Err(TagMapBuildError::UnknownFileKind);
        }
    }
    trace!(?paths, "Read directory");
    Ok(paths)
}

#[derive(Debug, thiserror::Error)]
#[allow(clippy::module_name_repetitions)]
/// Error returned when [`ETagMap::new`] fails.
pub enum TagMapBuildError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Could not strip prefix: {0}")]
    StripPrefix(#[from] std::path::StripPrefixError),
    #[error("Hex header value was somehow invalid")]
    InvalidHeaderValue(#[from] http::header::InvalidHeaderValue),
    #[error("ETagMap does not follow symlinks or other strange files")]
    UnknownFileKind,
    #[error("Path was not a valid UTF-8 string")]
    PathNotStr,
}
