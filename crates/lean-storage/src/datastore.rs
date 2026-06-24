use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{Error as ObjectStoreError, ObjectStore};

#[derive(Debug, Clone)]
pub struct S3StoreConfig {
    pub access_key: String,
    pub secret_key: String,
    pub bucket: String,
    pub endpoint: String,
    pub region: String,
    pub prefix: String,
    pub local_cache_root: PathBuf,
}

#[derive(Clone)]
pub enum DataStore {
    Local { root: PathBuf },
    S3(Arc<S3DataStore>),
}

pub struct S3DataStore {
    local_cache_root: PathBuf,
    prefix: String,
    store: Arc<dyn ObjectStore>,
}

impl std::fmt::Debug for DataStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataStore::Local { root } => f.debug_struct("Local").field("root", root).finish(),
            DataStore::S3(store) => f
                .debug_struct("S3")
                .field("local_cache_root", &store.local_cache_root)
                .field("prefix", &store.prefix)
                .finish_non_exhaustive(),
        }
    }
}

impl DataStore {
    pub fn local(root: impl Into<PathBuf>) -> Self {
        Self::Local { root: root.into() }
    }

    pub fn s3(config: S3StoreConfig) -> Result<Self> {
        if Path::new(&config.prefix).is_absolute() {
            bail!(
                "S3 data-folder must be a relative object prefix, got '{}'",
                config.prefix
            );
        }

        let store = AmazonS3Builder::new()
            .with_access_key_id(config.access_key)
            .with_secret_access_key(config.secret_key)
            .with_bucket_name(config.bucket)
            .with_endpoint(config.endpoint)
            .with_region(config.region)
            .with_virtual_hosted_style_request(false)
            .build()
            .context("failed to build S3 object store")?;

        Ok(Self::S3(Arc::new(S3DataStore {
            local_cache_root: config.local_cache_root,
            prefix: normalize_prefix(&config.prefix),
            store: Arc::new(store),
        })))
    }

    pub fn root(&self) -> &Path {
        match self {
            DataStore::Local { root } => root,
            DataStore::S3(store) => &store.local_cache_root,
        }
    }

    pub fn is_s3(&self) -> bool {
        matches!(self, DataStore::S3(_))
    }

    pub async fn materialize_path(&self, path: &Path) -> Result<bool> {
        match self {
            DataStore::Local { .. } => Ok(path.exists()),
            DataStore::S3(store) => store.materialize_path(path).await,
        }
    }

    pub async fn upload_path(&self, path: &Path) -> Result<()> {
        match self {
            DataStore::Local { .. } => Ok(()),
            DataStore::S3(store) => store.upload_path(path).await,
        }
    }

    pub async fn upload_tree(&self) -> Result<()> {
        let DataStore::S3(store) = self else {
            return Ok(());
        };
        let files = collect_files(&store.local_cache_root)?;
        for file in files {
            store.upload_path(&file).await?;
        }
        Ok(())
    }
}

impl S3DataStore {
    async fn materialize_path(&self, path: &Path) -> Result<bool> {
        if path.exists() {
            return Ok(true);
        }

        let object_path = self.object_path(path)?;
        let bytes = match self.store.get(&object_path).await {
            Ok(result) => result.bytes().await.with_context(|| {
                format!("failed to read S3 object body for {}", object_path.as_ref())
            })?,
            Err(ObjectStoreError::NotFound { .. }) => return Ok(false),
            Err(err) => {
                return Err(anyhow!(
                    "failed to fetch S3 object {}: {}",
                    object_path.as_ref(),
                    err
                ))
            }
        };

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        tokio::fs::write(path, bytes)
            .await
            .with_context(|| format!("failed to materialize {}", path.display()))?;
        Ok(true)
    }

    async fn upload_path(&self, path: &Path) -> Result<()> {
        if !path.exists() || !path.is_file() {
            return Ok(());
        }
        let object_path = self.object_path(path)?;
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        self.store
            .put(&object_path, Bytes::from(bytes).into())
            .await
            .with_context(|| format!("failed to upload S3 object {}", object_path.as_ref()))?;
        Ok(())
    }

    fn object_path(&self, path: &Path) -> Result<ObjectPath> {
        let relative = path.strip_prefix(&self.local_cache_root).with_context(|| {
            format!(
                "{} is not under S3 local cache root {}",
                path.display(),
                self.local_cache_root.display()
            )
        })?;
        let relative = path_to_object_key(relative)?;
        let key = if self.prefix.is_empty() {
            relative
        } else if relative.is_empty() {
            self.prefix.clone()
        } else {
            format!("{}/{}", self.prefix, relative)
        };
        Ok(ObjectPath::from(key))
    }
}

fn normalize_prefix(prefix: &str) -> String {
    prefix.trim_matches('/').to_string()
}

fn path_to_object_key(path: &Path) -> Result<String> {
    let parts = path
        .components()
        .map(|component| match component {
            std::path::Component::Normal(part) => part
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("path contains non-UTF-8 component: {}", path.display())),
            _ => Err(anyhow!(
                "path must be relative and normalized for S3 object key: {}",
                path.display()
            )),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join("/"))
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    collect_files_recursive(root, &mut out)?;
    Ok(out)
}

fn collect_files_recursive(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}
