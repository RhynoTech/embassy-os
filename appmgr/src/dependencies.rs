use anyhow::anyhow;
use emver::VersionRange;
use indexmap::IndexMap;
use patch_db::{DbHandle, DiffPatch, HasModel, Map, MapModel};
use serde::{Deserialize, Serialize};

use crate::action::ActionImplementation;
use crate::config::Config;
use crate::context::RpcContext;
use crate::db::model::CurrentDependencyInfo;
use crate::s9pk::manifest::PackageId;
use crate::status::health_check::{HealthCheckId, HealthCheckResult, HealthCheckResultVariant};
use crate::status::MainStatus;
use crate::util::Version;
use crate::volume::Volumes;
use crate::Error;

#[derive(Clone, Debug, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "type")]
pub enum DependencyError {
    NotInstalled, // { "type": "not-installed" }
    IncorrectVersion {
        expected: VersionRange,
        received: Version,
    }, // { "type": "incorrect-version", "expected": "0.1.0", "received": "^0.2.0" }
    ConfigUnsatisfied {
        error: String,
    }, // { "type": "config-unsatisfied", "error": "Bitcoin Core must have pruning set to manual." }
    NotRunning,   // { "type": "not-running" }
    HealthChecksFailed {
        failures: IndexMap<HealthCheckId, HealthCheckResult>,
    }, // { "type": "health-checks-failed", "checks": { "rpc": { "time": "2021-05-11T18:21:29Z", "result": "warming-up" } } }
}
impl DependencyError {
    pub fn merge_with(self, other: DependencyError) -> DependencyError {
        use DependencyError::*;
        match (self, other) {
            (NotInstalled, _) => NotInstalled,
            (_, NotInstalled) => NotInstalled,
            (IncorrectVersion { expected, received }, _) => IncorrectVersion { expected, received },
            (_, IncorrectVersion { expected, received }) => IncorrectVersion { expected, received },
            (ConfigUnsatisfied { error: e0 }, ConfigUnsatisfied { error: e1 }) => {
                ConfigUnsatisfied {
                    error: e0 + "\n" + &e1,
                }
            }
            (ConfigUnsatisfied { error }, _) => ConfigUnsatisfied { error },
            (_, ConfigUnsatisfied { error }) => ConfigUnsatisfied { error },
            (NotRunning, _) => NotRunning,
            (_, NotRunning) => NotRunning,
            (HealthChecksFailed { failures: f0 }, HealthChecksFailed { failures: f1 }) => {
                HealthChecksFailed {
                    failures: f0.into_iter().chain(f1.into_iter()).collect(),
                }
            }
        }
    }
}
impl std::fmt::Display for DependencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use DependencyError::*;
        match self {
            NotInstalled => write!(f, "Not Installed"),
            IncorrectVersion { expected, received } => write!(
                f,
                "Incorrect Version: Expected {}, Received {}",
                expected,
                received.as_str()
            ),
            ConfigUnsatisfied { error } => {
                write!(f, "Configuration Requirements Not Satisfied: {}", error)
            }
            NotRunning => write!(f, "Not Running"),
            HealthChecksFailed { failures } => {
                write!(f, "Failed Health Check(s): ")?;
                let mut comma = false;
                for (check, res) in failures {
                    if !comma {
                        comma = true;
                    } else {
                        write!(f, ", ")?;
                    }
                    write!(f, "{} @ {} {}", check, res.time, res.result)?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TaggedDependencyError {
    pub dependency: PackageId,
    pub error: DependencyError,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BreakageRes {
    pub patch: DiffPatch,
    pub breakages: IndexMap<PackageId, TaggedDependencyError>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Dependencies(pub IndexMap<PackageId, DepInfo>);
impl Map for Dependencies {
    type Key = PackageId;
    type Value = DepInfo;
    fn get(&self, key: &Self::Key) -> Option<&Self::Value> {
        self.0.get(key)
    }
}
impl HasModel for Dependencies {
    type Model = MapModel<Self>;
}

#[derive(Clone, Debug, Deserialize, Serialize, HasModel)]
#[serde(rename_all = "kebab-case")]
pub struct DepInfo {
    pub version: VersionRange,
    pub optional: Option<String>,
    pub recommended: bool,
    pub description: Option<String>,
    pub critical: bool,
    #[serde(default)]
    #[model]
    pub config: Option<DependencyConfig>,
}
impl DepInfo {
    pub async fn satisfied<Db: DbHandle>(
        &self,
        ctx: &RpcContext,
        db: &mut Db,
        dependency_id: &PackageId,
        dependency_config: Option<Config>, // fetch if none
        dependent_id: &PackageId,
        dependent_version: &Version,
        dependent_volumes: &Volumes,
    ) -> Result<Result<(), DependencyError>, Error> {
        let (manifest, info) = if let Some(dep_model) = crate::db::DatabaseModel::new()
            .package_data()
            .idx_model(dependency_id)
            .and_then(|pde| pde.installed())
            .check(db)
            .await?
        {
            (
                dep_model.clone().manifest().get(db, true).await?,
                dep_model.get(db, true).await?,
            )
        } else {
            return Ok(Err(DependencyError::NotInstalled));
        };
        if !&manifest.version.satisfies(&self.version) {
            return Ok(Err(DependencyError::IncorrectVersion {
                expected: self.version.clone(),
                received: manifest.version.clone(),
            }));
        }
        let dependency_config = if let Some(cfg) = dependency_config {
            cfg
        } else if let Some(cfg_info) = &manifest.config {
            cfg_info
                .get(ctx, dependency_id, &manifest.version, &manifest.volumes)
                .await?
                .config
                .unwrap_or_default()
        } else {
            Config::default()
        };
        if let Some(cfg_req) = &self.config {
            if let Err(e) = cfg_req
                .check(
                    ctx,
                    dependent_id,
                    dependent_version,
                    dependent_volumes,
                    &dependency_config,
                )
                .await
            {
                if e.kind == crate::ErrorKind::ConfigRulesViolation {
                    return Ok(Err(DependencyError::ConfigUnsatisfied {
                        error: format!("{}", e),
                    }));
                } else {
                    return Err(e);
                }
            }
        }
        match &info.status.main {
            MainStatus::BackingUp {
                started: Some(_),
                health,
            }
            | MainStatus::Running { health, .. } => {
                let mut failures = IndexMap::with_capacity(health.len());
                for (check, res) in health {
                    if !matches!(res.result, HealthCheckResultVariant::Success) {
                        failures.insert(check.clone(), res.clone());
                    }
                }
                if !failures.is_empty() {
                    return Ok(Err(DependencyError::HealthChecksFailed { failures }));
                }
            }
            _ => return Ok(Err(DependencyError::NotRunning)),
        }
        Ok(Ok(()))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, HasModel)]
#[serde(rename_all = "kebab-case")]
pub struct DependencyConfig {
    check: ActionImplementation,
    auto_configure: ActionImplementation,
}
impl DependencyConfig {
    pub async fn check(
        &self,
        ctx: &RpcContext,
        dependent_id: &PackageId,
        dependent_version: &Version,
        dependent_volumes: &Volumes,
        dependency_config: &Config,
    ) -> Result<Result<(), String>, Error> {
        Ok(self
            .check
            .sandboxed(
                ctx,
                dependent_id,
                dependent_version,
                dependent_volumes,
                Some(dependency_config),
            )
            .await?
            .map_err(|(_, e)| e))
    }
    pub async fn auto_configure(
        &self,
        ctx: &RpcContext,
        dependent_id: &PackageId,
        dependent_version: &Version,
        dependent_volumes: &Volumes,
        old: &Config,
    ) -> Result<Config, Error> {
        self.auto_configure
            .sandboxed(
                ctx,
                dependent_id,
                dependent_version,
                dependent_volumes,
                Some(old),
            )
            .await?
            .map_err(|e| Error::new(anyhow!("{}", e.1), crate::ErrorKind::AutoConfigure))
    }
}

pub async fn update_current_dependents<
    'a,
    Db: DbHandle,
    I: IntoIterator<Item = (&'a PackageId, &'a CurrentDependencyInfo)>,
>(
    db: &mut Db,
    dependent_id: &PackageId,
    current_dependencies: I,
) -> Result<(), Error> {
    for (dependency, dep_info) in current_dependencies {
        if let Some(dependency_model) = crate::db::DatabaseModel::new()
            .package_data()
            .idx_model(&dependency)
            .and_then(|pkg| pkg.installed())
            .check(db)
            .await?
        {
            dependency_model
                .current_dependents()
                .idx_model(dependent_id)
                .put(db, &dep_info)
                .await?;
        }
    }
    Ok(())
}
