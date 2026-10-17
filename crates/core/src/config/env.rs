use figment::{
    Metadata, Profile, Provider,
    util::nest,
    value::{Dict, Map, Value},
};

const PREFIX: &str = "ACTUP_";

/// Config keys set by `ACTUP_*` environment variables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvOverrides {
    keys: Vec<(String, String)>,
}

impl EnvOverrides {
    /// Map each `ACTUP_<A>_<B>` variable in `vars` to the config key `a.b`, matching the prefix in any case.
    pub fn from_vars<K, V>(vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let keys = vars
            .into_iter()
            .filter_map(|(name, value)| {
                config_key(name.as_ref()).map(|key| (key, value.as_ref().to_owned()))
            })
            .collect();
        Self { keys }
    }
}

fn config_key(name: &str) -> Option<String> {
    let name = name.trim();
    let rest = name
        .get(..PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
        .and_then(|_| name.get(PREFIX.len()..))?;
    let key = rest.replace('_', ".");
    let key = key.trim();
    if key.split('.').any(str::is_empty) {
        return None;
    }
    Some(key.to_ascii_lowercase())
}

fn merge(into: &mut Dict, from: Dict) {
    for (key, value) in from {
        match (into.get_mut(&key), value) {
            (Some(Value::Dict(_, existing)), Value::Dict(_, incoming)) => merge(existing, incoming),
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

impl Provider for EnvOverrides {
    fn metadata(&self) -> Metadata {
        Metadata::named(format!("`{PREFIX}` environment variable(s)")).interpolater(
            |_: &Profile, keys: &[&str]| {
                keys.iter()
                    .map(|key| key.to_ascii_uppercase())
                    .collect::<Vec<_>>()
                    .join(".")
            },
        )
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut dict = Dict::new();
        for (key, raw) in &self.keys {
            let Ok(value) = raw.parse::<Value>();
            if let Some(nested) = nest(key, value).into_dict() {
                merge(&mut dict, nested);
            }
        }
        Ok(Profile::Default.collect(dict))
    }
}

#[cfg(test)]
mod tests {
    use figment::{Figment, providers::Env};

    use super::*;

    fn keys(vars: &[(&str, &str)]) -> Vec<(String, String)> {
        EnvOverrides::from_vars(vars.iter().copied()).keys
    }

    fn pair(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    #[test]
    fn from_vars_maps_prefixed_names_to_dotted_lowercase_keys() {
        assert_eq!(
            keys(&[
                ("ACTUP_POLICY_BUMP", "patch"),
                ("actup_scan_ignore", "[\"a/b\"]"),
                ("Actup_Cache_Enabled", "false"),
                (" ACTUP_DEFAULT_HOST ", "ghe.corp.com"),
                ("ACTUP_VERBOSE", "1"),
                ("ACTUP_FORMAT", "json"),
                ("ACTUP_STRICT", "true"),
            ]),
            [
                pair("policy.bump", "patch"),
                pair("scan.ignore", "[\"a/b\"]"),
                pair("cache.enabled", "false"),
                pair("default.host", "ghe.corp.com"),
                pair("verbose", "1"),
                pair("format", "json"),
                pair("strict", "true"),
            ]
        );
    }

    #[test]
    fn from_vars_skips_unprefixed_and_empty_segment_names() {
        assert_eq!(
            keys(&[
                ("GITHUB_TOKEN", "t"),
                ("ACTUP", "x"),
                ("ACTUP_", "x"),
                ("ACTUP__POLICY", "x"),
                ("ACTUP_POLICY_", "x"),
                ("ACTUP_POLICY__BUMP", "x"),
                ("ACTUPé", "x"),
                ("", "x"),
                ("XACTUP_POLICY_BUMP", "x"),
            ]),
            Vec::<(String, String)>::new()
        );
    }

    #[test]
    fn data_nests_keys_and_later_values_win() -> Result<(), Box<dyn std::error::Error>> {
        let env = EnvOverrides::from_vars([
            ("ACTUP_POLICY_BUMP", "minor"),
            ("ACTUP_POLICY_PRERELEASE", "true"),
            ("actup_policy_bump", "patch"),
            ("ACTUP_FETCH_MAX_TAGS_PER_REPO", "7"),
        ]);
        let value: Value = Figment::from(env).extract()?;
        assert_eq!(
            value.find_ref("policy.bump").and_then(Value::as_str),
            Some("patch")
        );
        assert_eq!(
            value.find_ref("policy.prerelease").and_then(Value::to_bool),
            Some(true)
        );
        assert_eq!(
            value
                .find_ref("fetch.max.tags.per.repo")
                .and_then(Value::to_u128),
            Some(7)
        );
        Ok(())
    }

    #[test]
    fn data_lets_a_later_leaf_replace_a_table_and_back() -> Result<(), Box<dyn std::error::Error>> {
        let env = EnvOverrides::from_vars([
            ("ACTUP_POLICY_BUMP", "minor"),
            ("ACTUP_POLICY", "flat"),
            ("ACTUP_CACHE", "flat"),
            ("ACTUP_CACHE_TTL", "60"),
        ]);
        let value: Value = Figment::from(env).extract()?;
        assert_eq!(
            value.find_ref("policy").and_then(Value::as_str),
            Some("flat")
        );
        assert_eq!(
            value.find_ref("cache.ttl").and_then(Value::to_u128),
            Some(60)
        );
        Ok(())
    }

    const PARITY_CHILD: &str = "ENV_PARITY_CHILD";

    const PARITY_VARS: &[(&str, &str)] = &[
        ("ACTUP_POLICY_BUMP", "minor"),
        ("actup_policy", "flat"),
        ("actup_policy_prerelease", "true"),
        ("ACTUP_CACHE", "flat"),
        ("ACTUP_CACHE_TTL", "60"),
        ("Actup_Cache_Ttl", "90"),
        (" ACTUP_DEFAULT_HOST ", "ghe.corp.com"),
        ("ACTUP_SCAN_IGNORE", "[\"a/b\", \"c/d\"]"),
        ("ACTUP_META", "{a = 1, b = \"two\"}"),
        ("ACTUP_FETCH_MAX_TAGS_PER_REPO", "7"),
        ("ACTUP_", "x"),
        ("ACTUP__POLICY", "x"),
        ("ACTUP_POLICY_", "x"),
        ("ACTUP_POLICY__BUMP", "x"),
        ("XACTUP_POLICY_BUMP", "x"),
        ("GITHUB_TOKEN", "t"),
    ];

    #[test]
    fn matches_figment_env_on_a_controlled_environment() -> Result<(), Box<dyn std::error::Error>> {
        if std::env::var_os(PARITY_CHILD).is_some() {
            let vars = std::env::vars_os().map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            });
            let ours: Value = Figment::from(EnvOverrides::from_vars(vars)).extract()?;
            let figment: Value = Figment::from(Env::prefixed(PREFIX).split("_")).extract()?;
            assert_eq!(ours, figment);
            assert!(ours.as_dict().is_some_and(|dict| !dict.is_empty()));
            return Ok(());
        }

        let path = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, path)| path);
        let name = format!("{path}::matches_figment_env_on_a_controlled_environment");
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", &name])
            .env_clear()
            .env(PARITY_CHILD, "1")
            .envs(PARITY_VARS.iter().copied())
            .output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}{stderr}");
        assert!(stdout.contains("1 passed"), "{stdout}{stderr}");
        Ok(())
    }

    #[test]
    fn metadata_names_the_prefix_and_upper_cases_keys() {
        let metadata = EnvOverrides::default().metadata();
        assert_eq!(metadata.name, "`ACTUP_` environment variable(s)");
        assert_eq!(
            metadata.interpolate(&Profile::Default, &["policy", "bump"]),
            "POLICY.BUMP"
        );
    }
}
