use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use url::Url;

use crate::resource::ContentRevision;

use super::PackageValidationError;

#[derive(Clone, Copy, Debug)]
pub struct SourceLimits {
    pub download_bytes: usize,
    pub extracted_bytes: usize,
    pub files: usize,
    pub redirects: usize,
    pub timeout: Duration,
}

impl Default for SourceLimits {
    fn default() -> Self {
        Self {
            download_bytes: 32 * 1024 * 1024,
            extracted_bytes: 128 * 1024 * 1024,
            files: 4096,
            redirects: 4,
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedSource {
    pub root: PathBuf,
    pub revision: String,
    pub digest: ContentRevision,
    pub origin: String,
}

pub struct SourceResolver {
    staging_root: PathBuf,
    limits: SourceLimits,
}

impl SourceResolver {
    pub fn new(staging_root: impl Into<PathBuf>, limits: SourceLimits) -> Self {
        Self {
            staging_root: staging_root.into(),
            limits,
        }
    }

    pub fn resolve(&self, source: &str) -> Result<ResolvedSource, PackageValidationError> {
        let started = std::time::Instant::now();
        fs::create_dir_all(&self.staging_root).map_err(io_error)?;
        let root = unique_directory(&self.staging_root, "source")?;
        let result = if let Some(specification) = source.strip_prefix("git+") {
            self.resolve_git(specification, &root, source)
        } else if let Some(specification) = source.strip_prefix("github:") {
            let (repository, revision) = optional_revision(specification);
            let specification = revision.map_or_else(
                || format!("https://github.com/{repository}.git"),
                |revision| format!("https://github.com/{repository}.git#{revision}"),
            );
            self.resolve_git(&specification, &root, source)
        } else if source.starts_with("https://") || source.starts_with("http://") {
            self.resolve_url(source, &root)
        } else {
            self.resolve_path(Path::new(source), &root)
        };
        if result.is_err() {
            let _ = fs::remove_dir_all(&root);
        }
        crate::observability::emit(crate::observability::EventInput {
            level: if result.is_ok() {
                crate::observability::LogLevel::Info
            } else {
                crate::observability::LogLevel::Warn
            },
            target: "workflow.package",
            action: "resolve",
            operation_id: None,
            parent_operation_id: None,
            branch: None,
            session: None,
            message: format!(
                "package resolution {}",
                if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                }
            ),
            data_json: Some(
                serde_json::json!({
                    "source_kind": source_kind(source),
                    "elapsed_ms": i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                    "succeeded": result.is_ok(),
                })
                .to_string(),
            ),
        });
        result
    }

    fn resolve_path(
        &self,
        source: &Path,
        root: &Path,
    ) -> Result<ResolvedSource, PackageValidationError> {
        if !source.is_dir() {
            return Err(PackageValidationError::InvalidField(format!(
                "package path {} is not a directory",
                source.display()
            )));
        }
        copy_tree(source, root, self.limits)?;
        let canonical = canonical_tree(root, self.limits)?;
        Ok(ResolvedSource {
            root: root.into(),
            revision: ContentRevision::digest(&canonical).to_string(),
            digest: ContentRevision::digest(&canonical),
            origin: fs::canonicalize(source)
                .map_err(io_error)?
                .to_string_lossy()
                .into_owned(),
        })
    }

    fn resolve_url(
        &self,
        source: &str,
        root: &Path,
    ) -> Result<ResolvedSource, PackageValidationError> {
        let mut current = Url::parse(source)
            .map_err(|error| PackageValidationError::InvalidField(error.to_string()))?;
        if current.scheme() != "https" {
            return Err(PackageValidationError::InvalidField(
                "URL package sources must use HTTPS".into(),
            ));
        }
        let original_origin = origin(&current);
        let transport_config = ureq::config::Config::builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(self.limits.timeout))
            .build();
        let agent = transport_config.new_agent();
        let mut redirects = 0;
        let archive = loop {
            let response = agent.get(current.as_str()).call().map_err(|error| {
                PackageValidationError::InvalidField(format!("package download failed: {error}"))
            })?;
            let status = response.status().as_u16();
            if (300..400).contains(&status) {
                if redirects >= self.limits.redirects {
                    return Err(PackageValidationError::InvalidField(
                        "too many package redirects".into(),
                    ));
                }
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        PackageValidationError::InvalidField(
                            "package redirect has no location".into(),
                        )
                    })?;
                let next = current
                    .join(location)
                    .map_err(|error| PackageValidationError::InvalidField(error.to_string()))?;
                if origin(&next) != original_origin {
                    return Err(PackageValidationError::InvalidField(
                        "package redirect changed origin".into(),
                    ));
                }
                current = next;
                redirects += 1;
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(PackageValidationError::InvalidField(format!(
                    "package download returned HTTP {status}"
                )));
            }
            break read_bounded(
                response.into_body().into_reader(),
                self.limits.download_bytes,
            )?;
        };
        self.extract_archive(&archive, root)?;
        let canonical = canonical_tree(root, self.limits)?;
        Ok(ResolvedSource {
            root: root.into(),
            revision: ContentRevision::digest(&archive).to_string(),
            digest: ContentRevision::digest(&canonical),
            origin: source.into(),
        })
    }

    fn extract_archive(&self, archive: &[u8], root: &Path) -> Result<(), PackageValidationError> {
        let tar = if archive.starts_with(&[0x1f, 0x8b]) {
            decompress_gzip(
                archive,
                self.limits.extracted_bytes.saturating_add(1024 * 1024),
            )?
        } else {
            archive.to_vec()
        };
        extract_tar(&tar, root, self.limits)?;
        flatten_single_root(root)
    }

    fn resolve_git(
        &self,
        specification: &str,
        root: &Path,
        original: &str,
    ) -> Result<ResolvedSource, PackageValidationError> {
        let (url, expected_revision) = optional_revision(specification);
        if let Some(revision) = expected_revision
            && (revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(PackageValidationError::InvalidField(
                "a pinned Git package revision must be a 40-character commit".into(),
            ));
        }
        run_git(
            root.parent().expect("staging root"),
            &["init", root.to_str().unwrap_or_default()],
        )?;
        run_git(root, &["remote", "add", "origin", url])?;
        run_git(
            root,
            &[
                "fetch",
                "--depth=1",
                "origin",
                expected_revision.unwrap_or("HEAD"),
            ],
        )?;
        run_git(root, &["checkout", "--detach", "FETCH_HEAD"])?;
        let actual = git_output(root, &["rev-parse", "HEAD"])?;
        if let Some(expected) = expected_revision
            && actual.trim() != expected
        {
            return Err(PackageValidationError::InvalidField(format!(
                "Git resolved {actual}, expected {expected}"
            )));
        }
        let git = root.join(".git");
        fs::remove_dir_all(git).map_err(io_error)?;
        let canonical = canonical_tree(root, self.limits)?;
        Ok(ResolvedSource {
            root: root.into(),
            revision: actual,
            digest: ContentRevision::digest(&canonical),
            origin: original.into(),
        })
    }
}

fn source_kind(source: &str) -> &'static str {
    if source.starts_with("git+") || source.starts_with("github:") {
        "git"
    } else if source.starts_with("https://") || source.starts_with("http://") {
        "url"
    } else {
        "path"
    }
}

fn origin(url: &Url) -> (String, Option<String>, Option<u16>) {
    (
        url.scheme().into(),
        url.host_str().map(str::to_ascii_lowercase),
        url.port_or_known_default(),
    )
}

fn optional_revision(value: &str) -> (&str, Option<&str>) {
    value
        .rsplit_once('#')
        .map_or((value, None), |(source, revision)| {
            (source, (!revision.is_empty()).then_some(revision))
        })
}

fn unique_directory(parent: &Path, label: &str) -> Result<PathBuf, PackageValidationError> {
    for suffix in 0..1000_u32 {
        let path = parent.join(format!(".{label}-{}-{suffix}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    Err(PackageValidationError::InvalidField(
        "could not allocate package staging directory".into(),
    ))
}

pub(crate) fn canonical_tree(
    root: &Path,
    limits: SourceLimits,
) -> Result<Vec<u8>, PackageValidationError> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files, limits)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut bytes = Vec::new();
    for (path, content) in files {
        bytes.extend_from_slice(&(path.len() as u64).to_be_bytes());
        bytes.extend_from_slice(path.as_bytes());
        bytes.extend_from_slice(&(content.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&content);
    }
    Ok(bytes)
}

fn collect_files(
    root: &Path,
    directory: &Path,
    output: &mut Vec<(String, Vec<u8>)>,
    limits: SourceLimits,
) -> Result<(), PackageValidationError> {
    for entry in fs::read_dir(directory).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let file_type = entry.file_type().map_err(io_error)?;
        if file_type.is_symlink() {
            return Err(PackageValidationError::UnsafePath(
                entry.path().display().to_string(),
            ));
        }
        if file_type.is_dir() {
            collect_files(root, &entry.path(), output, limits)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(PackageValidationError::UnsafePath(
                entry.path().display().to_string(),
            ));
        }
        if output.len() >= limits.files {
            return Err(PackageValidationError::InvalidField(
                "package contains too many files".into(),
            ));
        }
        let content = fs::read(entry.path()).map_err(io_error)?;
        let total = output
            .iter()
            .map(|(_, value)| value.len())
            .sum::<usize>()
            .saturating_add(content.len());
        if total > limits.extracted_bytes {
            return Err(PackageValidationError::InvalidField(
                "package extracted size limit exceeded".into(),
            ));
        }
        let path = entry
            .path()
            .strip_prefix(root)
            .expect("walked under root")
            .to_string_lossy()
            .replace('\\', "/");
        output.push((path, content));
    }
    Ok(())
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    limits: SourceLimits,
) -> Result<(), PackageValidationError> {
    let canonical = canonical_tree(source, limits)?;
    let _ = canonical;
    for entry in fs::read_dir(source).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let target = destination.join(entry.file_name());
        let file_type = entry.file_type().map_err(io_error)?;
        if file_type.is_dir() {
            fs::create_dir(&target).map_err(io_error)?;
            copy_tree(&entry.path(), &target, limits)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), target).map_err(io_error)?;
        } else {
            return Err(PackageValidationError::UnsafePath(
                entry.path().display().to_string(),
            ));
        }
    }
    Ok(())
}

fn read_bounded(mut reader: impl Read, limit: usize) -> Result<Vec<u8>, PackageValidationError> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > limit {
        return Err(PackageValidationError::InvalidField(
            "package download size limit exceeded".into(),
        ));
    }
    Ok(bytes)
}

fn decompress_gzip(bytes: &[u8], limit: usize) -> Result<Vec<u8>, PackageValidationError> {
    let mut child = Command::new("gzip")
        .arg("-dc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(io_error)?;
    let input = bytes.to_vec();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = read_bounded(child.stdout.take().expect("piped stdout"), limit)?;
    writer
        .join()
        .map_err(|_| PackageValidationError::InvalidField("gzip input writer panicked".into()))?
        .map_err(io_error)?;
    let status = child.wait().map_err(io_error)?;
    if !status.success() {
        return Err(PackageValidationError::InvalidField(
            "invalid gzip package archive".into(),
        ));
    }
    Ok(output)
}

fn extract_tar(
    bytes: &[u8],
    root: &Path,
    limits: SourceLimits,
) -> Result<(), PackageValidationError> {
    let mut offset = 0;
    let mut count = 0;
    let mut total = 0_usize;
    while offset + 512 <= bytes.len() {
        let header = &bytes[offset..offset + 512];
        offset += 512;
        if header.iter().all(|byte| *byte == 0) {
            return Ok(());
        }
        let expected_checksum = parse_octal(&header[148..156])?;
        let actual_checksum = header[..148]
            .iter()
            .chain([b' '; 8].iter())
            .chain(header[156..].iter())
            .map(|byte| usize::from(*byte))
            .sum::<usize>();
        if expected_checksum != actual_checksum {
            return Err(PackageValidationError::InvalidField(
                "invalid tar header checksum".into(),
            ));
        }
        let name = c_string(&header[..100]);
        let prefix = c_string(&header[345..500]);
        let name = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        super::manifest::validate_relative_path(&name)?;
        let size = parse_octal(&header[124..136])?;
        let kind = header[156];
        if size > bytes.len().saturating_sub(offset) {
            return Err(PackageValidationError::InvalidField(
                "truncated tar archive".into(),
            ));
        }
        count += 1;
        if count > limits.files {
            return Err(PackageValidationError::InvalidField(
                "package contains too many archive entries".into(),
            ));
        }
        let target = root.join(&name);
        match kind {
            0 | b'0' => {
                total = total.saturating_add(size);
                if total > limits.extracted_bytes {
                    return Err(PackageValidationError::InvalidField(
                        "package extracted size limit exceeded".into(),
                    ));
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(io_error)?;
                }
                fs::write(target, &bytes[offset..offset + size]).map_err(io_error)?;
            }
            b'5' => fs::create_dir_all(target).map_err(io_error)?,
            _ => {
                return Err(PackageValidationError::UnsafePath(format!(
                    "archive entry {name} has unsupported type {kind}"
                )));
            }
        }
        offset = offset
            .checked_add(size.div_ceil(512) * 512)
            .ok_or_else(|| PackageValidationError::InvalidField("tar size overflow".into()))?;
    }
    Err(PackageValidationError::InvalidField(
        "tar archive has no end marker".into(),
    ))
}

fn flatten_single_root(root: &Path) -> Result<(), PackageValidationError> {
    let entries = fs::read_dir(root)
        .map_err(io_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(io_error)?;
    if entries.len() != 1 || !entries[0].file_type().map_err(io_error)?.is_dir() {
        return Ok(());
    }
    let nested = entries[0].path();
    let children = fs::read_dir(&nested)
        .map_err(io_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(io_error)?;
    for entry in children {
        fs::rename(entry.path(), root.join(entry.file_name())).map_err(io_error)?;
    }
    fs::remove_dir(nested).map_err(io_error)
}

fn c_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(
        &bytes[..bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len())],
    )
    .into_owned()
}
fn parse_octal(bytes: &[u8]) -> Result<usize, PackageValidationError> {
    usize::from_str_radix(c_string(bytes).trim(), 8)
        .map_err(|_| PackageValidationError::InvalidField("invalid tar size".into()))
}

fn run_git(directory: &Path, arguments: &[&str]) -> Result<(), PackageValidationError> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(io_error)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(PackageValidationError::InvalidField(format!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
fn git_output(directory: &Path, arguments: &[&str]) -> Result<String, PackageValidationError> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()
        .map_err(io_error)?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().into())
    } else {
        Err(PackageValidationError::InvalidField(
            "git revision lookup failed".into(),
        ))
    }
}
fn io_error(error: std::io::Error) -> PackageValidationError {
    PackageValidationError::InvalidField(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_source_rejects_symlink_escape() {
        let root = std::env::temp_dir().join(format!("prism-source-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("input")).unwrap();
        fs::write(root.join("outside"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("outside"), root.join("input/link")).unwrap();
        let resolver = SourceResolver::new(root.join("stage"), SourceLimits::default());
        assert!(matches!(
            resolver.resolve(root.join("input").to_str().unwrap()),
            Err(PackageValidationError::UnsafePath(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unpinned_git_source_is_resolved_to_an_exact_commit() {
        let root = std::env::temp_dir().join(format!("prism-git-source-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let repository = root.join("repository");
        fs::create_dir_all(&repository).unwrap();
        run_git(&repository, &["init"]).unwrap();
        run_git(&repository, &["config", "user.name", "Prism Test"]).unwrap();
        run_git(
            &repository,
            &["config", "user.email", "prism@example.invalid"],
        )
        .unwrap();
        fs::write(repository.join("package.toml"), "source").unwrap();
        run_git(&repository, &["add", "package.toml"]).unwrap();
        run_git(&repository, &["commit", "-m", "fixture"]).unwrap();
        let expected = git_output(&repository, &["rev-parse", "HEAD"]).unwrap();
        let resolver = SourceResolver::new(root.join("stage"), SourceLimits::default());
        let resolved = resolver
            .resolve(&format!("git+file://{}", repository.display()))
            .unwrap();
        assert_eq!(resolved.revision, expected);
        assert!(!resolved.root.join(".git").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tar_extraction_rejects_parent_traversal() {
        let root = std::env::temp_dir().join(format!("prism-tar-source-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let archive = tar_file("../escaped", b"bad");
        assert!(matches!(
            extract_tar(&archive, &root, SourceLimits::default()),
            Err(PackageValidationError::UnsafePath(_))
        ));
        assert!(!root.parent().unwrap().join("escaped").exists());
        fs::remove_dir_all(root).unwrap();
    }

    fn tar_file(name: &str, content: &[u8]) -> Vec<u8> {
        let mut header = [0_u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", content.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: usize = header.iter().map(|byte| usize::from(*byte)).sum();
        let checksum = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        let mut archive = header.to_vec();
        archive.extend_from_slice(content);
        archive.resize(512 + content.len().div_ceil(512) * 512, 0);
        archive.resize(archive.len() + 1024, 0);
        archive
    }
}
