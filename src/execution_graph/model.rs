#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplexityClassificationStage {
    Provisional,
    #[default]
    Authoritative,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionComplexity {
    #[default]
    Tiny,
    Small,
    Medium,
    Large,
}

impl MissionComplexity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tiny => "tiny",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    pub fn default_budget(self) -> MissionBudget {
        MissionBudget::for_complexity(self)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct MissionBudget {
    pub max_model_calls: u32,
    pub max_cost_micros: u64,
    #[serde(with = "duration_millis")]
    pub max_duration: Duration,
    pub max_target_repair_rounds: u32,
}

impl MissionBudget {
    pub const TINY_MAX_COST_MICROS: u64 = 2_000_000;
    pub const SMALL_MAX_COST_MICROS: u64 = 5_000_000;
    pub const MEDIUM_MAX_COST_MICROS: u64 = 10_000_000;
    pub const LARGE_MAX_COST_MICROS: u64 = 20_000_000;

    pub fn for_complexity(complexity: MissionComplexity) -> Self {
        match complexity {
            MissionComplexity::Tiny => Self {
                max_model_calls: 14,
                max_cost_micros: Self::TINY_MAX_COST_MICROS,
                max_duration: Duration::from_secs(8 * 60),
                max_target_repair_rounds: 1,
            },
            MissionComplexity::Small => Self {
                max_model_calls: 25,
                max_cost_micros: Self::SMALL_MAX_COST_MICROS,
                max_duration: Duration::from_secs(15 * 60),
                max_target_repair_rounds: 2,
            },
            MissionComplexity::Medium => Self {
                max_model_calls: 45,
                max_cost_micros: Self::MEDIUM_MAX_COST_MICROS,
                max_duration: Duration::from_secs(35 * 60),
                max_target_repair_rounds: 3,
            },
            MissionComplexity::Large => Self {
                max_model_calls: 80,
                max_cost_micros: Self::LARGE_MAX_COST_MICROS,
                max_duration: Duration::from_secs(75 * 60),
                max_target_repair_rounds: 4,
            },
        }
    }

    pub fn applying_override(&self, policy: &MissionBudgetOverride) -> Self {
        Self {
            max_model_calls: policy.max_model_calls.unwrap_or(self.max_model_calls),
            max_cost_micros: policy.max_cost_micros.unwrap_or(self.max_cost_micros),
            max_duration: policy.max_duration.unwrap_or(self.max_duration),
            max_target_repair_rounds: policy
                .max_target_repair_rounds
                .unwrap_or(self.max_target_repair_rounds),
        }
    }
}

impl Default for MissionBudget {
    fn default() -> Self {
        Self::for_complexity(MissionComplexity::Tiny)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct MissionBudgetOverride {
    pub max_model_calls: Option<u32>,
    pub max_cost_micros: Option<u64>,
    #[serde(default, with = "optional_duration_millis")]
    pub max_duration: Option<Duration>,
    pub max_target_repair_rounds: Option<u32>,
}

mod optional_duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<u64>::deserialize(deserializer).map(|value| value.map(Duration::from_millis))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComplexityFactorKind {
    PlannedTargetCount,
    NewFileCount,
    RepositoryCount,
    DependencyChanges,
    DatabaseSchemaChanges,
    ExternalIntegrations,
    SecuritySensitiveChanges,
    ArchitecturalUncertainty,
    TestSurface,
    ExpectedValidationDuration,
    CrossModuleImpact,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ComplexityFactor {
    pub kind: ComplexityFactorKind,
    pub value: u64,
    pub score: u32,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ComplexityInput {
    pub planned_target_count: u32,
    pub new_file_count: u32,
    pub repository_count: u32,
    pub dependency_change_count: u32,
    pub database_schema_change_count: u32,
    pub external_integration_count: u32,
    pub security_sensitive_change_count: u32,
    pub architectural_uncertainty: u32,
    pub test_surface: u32,
    #[serde(with = "duration_millis")]
    pub expected_validation_duration: Duration,
    pub cross_module_impact: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ComplexityAssessment {
    #[serde(default)]
    pub stage: ComplexityClassificationStage,
    pub class: MissionComplexity,
    pub score: u32,
    pub factors: Vec<ComplexityFactor>,
    pub budget: MissionBudget,
}

impl ComplexityAssessment {
    pub fn classify(input: &ComplexityInput) -> Self {
        classify_complexity(input, None)
    }

    pub fn classify_with_policy(input: &ComplexityInput, policy: &MissionBudgetOverride) -> Self {
        classify_complexity(input, Some(policy))
    }
}

pub fn classify_complexity(
    input: &ComplexityInput,
    policy: Option<&MissionBudgetOverride>,
) -> ComplexityAssessment {
    let target_score = input.planned_target_count.saturating_sub(1).min(8);
    let new_file_score = input.new_file_count.min(4);
    let repository_score = input.repository_count.saturating_sub(1).saturating_mul(4);
    let dependency_score = input.dependency_change_count.saturating_mul(4).min(8);
    let schema_score = input.database_schema_change_count.saturating_mul(7).min(14);
    let integration_score = input.external_integration_count.saturating_mul(5).min(10);
    let security_score = input
        .security_sensitive_change_count
        .saturating_mul(6)
        .min(12);
    let uncertainty_score = input.architectural_uncertainty.min(5).saturating_mul(2);
    let test_score = match input.test_surface {
        0..=2 => 0,
        3..=7 => 2,
        8..=15 => 4,
        _ => 7,
    };
    let validation_minutes = input.expected_validation_duration.as_secs().div_ceil(60);
    let validation_score = match validation_minutes {
        0..=5 => 0,
        6..=15 => 2,
        16..=30 => 4,
        _ => 7,
    };
    let cross_module_score = input.cross_module_impact.saturating_mul(2).min(8);

    let raw_factors = [
        (
            ComplexityFactorKind::PlannedTargetCount,
            u64::from(input.planned_target_count),
            target_score,
            "planned targets",
        ),
        (
            ComplexityFactorKind::NewFileCount,
            u64::from(input.new_file_count),
            new_file_score,
            "new files",
        ),
        (
            ComplexityFactorKind::RepositoryCount,
            u64::from(input.repository_count),
            repository_score,
            "repositories",
        ),
        (
            ComplexityFactorKind::DependencyChanges,
            u64::from(input.dependency_change_count),
            dependency_score,
            "dependency changes",
        ),
        (
            ComplexityFactorKind::DatabaseSchemaChanges,
            u64::from(input.database_schema_change_count),
            schema_score,
            "database or schema changes",
        ),
        (
            ComplexityFactorKind::ExternalIntegrations,
            u64::from(input.external_integration_count),
            integration_score,
            "external integrations",
        ),
        (
            ComplexityFactorKind::SecuritySensitiveChanges,
            u64::from(input.security_sensitive_change_count),
            security_score,
            "security-sensitive changes",
        ),
        (
            ComplexityFactorKind::ArchitecturalUncertainty,
            u64::from(input.architectural_uncertainty),
            uncertainty_score,
            "architectural uncertainty",
        ),
        (
            ComplexityFactorKind::TestSurface,
            u64::from(input.test_surface),
            test_score,
            "test surface",
        ),
        (
            ComplexityFactorKind::ExpectedValidationDuration,
            validation_minutes,
            validation_score,
            "expected validation minutes",
        ),
        (
            ComplexityFactorKind::CrossModuleImpact,
            u64::from(input.cross_module_impact),
            cross_module_score,
            "cross-module impact",
        ),
    ];
    let factors = raw_factors
        .into_iter()
        .map(|(kind, value, score, detail)| ComplexityFactor {
            kind,
            value,
            score,
            detail: detail.to_owned(),
        })
        .collect::<Vec<_>>();
    let score = factors.iter().map(|factor| factor.score).sum();
    let class = match score {
        0..=2 => MissionComplexity::Tiny,
        3..=9 => MissionComplexity::Small,
        10..=19 => MissionComplexity::Medium,
        _ => MissionComplexity::Large,
    };
    let default_budget = MissionBudget::for_complexity(class);
    let budget = policy.map_or(default_budget.clone(), |value| {
        default_budget.applying_override(value)
    });

    ComplexityAssessment {
        stage: ComplexityClassificationStage::Authoritative,
        class,
        score,
        factors,
        budget,
    }
}
