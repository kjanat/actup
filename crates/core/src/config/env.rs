use figment::{
    Metadata, Profile, Provider,
    value::{Dict, Map, Value},
};

const PREFIX: &str = "ACTUP_";

const SEPARATOR: &str = "__";

/// Environment variable that names the explicit config file.
pub const CONFIG_VAR: &str = "ACTUP_CONFIG";

/// Environment variable that sets the CLI `--verbose` flag.
pub const VERBOSE_VAR: &str = "ACTUP_VERBOSE";

/// Environment variable that sets the CLI `--format` option.
pub const FORMAT_VAR: &str = "ACTUP_FORMAT";

/// Environment variable that sets the CLI `--strict` flag.
pub const STRICT_VAR: &str = "ACTUP_STRICT";

/// Environment variable that sets the CLI `--color` option.
pub const COLOR_VAR: &str = "ACTUP_COLOR";

/// Environment variables that set CLI flags and never set a config key.
pub const CLI_FLAG_VARS: [&str; 5] = [CONFIG_VAR, VERBOSE_VAR, FORMAT_VAR, STRICT_VAR, COLOR_VAR];

/// Config keys set by `ACTUP_*` environment variables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvOverrides {
    keys: Vec<(Key, String)>,
    names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    head: String,
    tail: Vec<String>,
}

impl EnvOverrides {
    /// Map each `ACTUP_<A>__<B_C>` variable in `vars` to the lowercase config key `a.b_c`, skipping [`CLI_FLAG_VARS`] and names with an empty level.
    pub fn from_vars<K, V>(vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut keys = Vec::new();
        let mut names = Vec::new();
        for (name, value) in vars {
            if let Some(key) = config_key(name.as_ref()) {
                keys.push((key, value.as_ref().to_owned()));
                names.push(name.as_ref().trim().to_owned());
            }
        }
        names.sort();
        names.dedup();
        Self { keys, names }
    }

    /// Return the names of the variables that set a config key, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.names.clone()
    }
}

fn config_key(name: &str) -> Option<Key> {
    let name = name.trim();
    if CLI_FLAG_VARS
        .iter()
        .any(|var| var.eq_ignore_ascii_case(name))
    {
        return None;
    }
    let rest = name
        .get(..PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
        .and_then(|_| name.get(PREFIX.len()..))?;
    let mut levels = rest.split(SEPARATOR).map(str::to_ascii_lowercase);
    let head = levels.next().filter(|level| !level.is_empty())?;
    let tail: Vec<String> = levels.collect();
    if tail.iter().any(String::is_empty) {
        return None;
    }
    Some(Key { head, tail })
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
                let name = keys
                    .iter()
                    .map(|key| key.to_ascii_uppercase())
                    .collect::<Vec<_>>()
                    .join(SEPARATOR);
                format!("{PREFIX}{name}")
            },
        )
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut dict = Dict::new();
        for (key, raw) in &self.keys {
            let Ok(value) = raw.parse::<Value>();
            let value = key.tail.iter().rev().fold(value, |value, level| {
                Value::from(Dict::from([(level.clone(), value)]))
            });
            merge(&mut dict, Dict::from([(key.head.clone(), value)]));
        }
        Ok(Profile::Default.collect(dict))
    }
}

#[cfg(test)]
mod tests {
    use figment::Figment;

    use super::*;

    fn keys(vars: &[(&str, &str)]) -> Vec<(String, String)> {
        EnvOverrides::from_vars(vars.iter().copied())
            .keys
            .into_iter()
            .map(|(key, value)| {
                let path: Vec<String> = std::iter::once(key.head).chain(key.tail).collect();
                (path.join("."), value)
            })
            .collect()
    }

    fn pair(key: &str, value: &str) -> (String, String) {
        (key.to_owned(), value.to_owned())
    }

    #[test]
    fn from_vars_splits_levels_on_double_underscore() {
        assert_eq!(
            keys(&[
                ("ACTUP_POLICY__BUMP", "patch"),
                ("actup_scan__extra_paths", "[]"),
                ("Actup_Cache__Ttl", "60"),
                (" ACTUP_DEFAULT_HOST ", "ghe.corp.com"),
                ("ACTUP_FETCH__MAX_TAGS_PER_REPO", "7"),
                ("ACTUP_HOSTS__GHE.CORP.COM__TOKEN_ENV", "GHE_TOKEN"),
                ("ACTUP_POLICY_BUMP", "minor"),
                ("ACTUP__POLICY", "x"),
                ("ACTUP_POLICY___BUMP", "x"),
            ]),
            [
                pair("policy.bump", "patch"),
                pair("scan.extra_paths", "[]"),
                pair("cache.ttl", "60"),
                pair("default_host", "ghe.corp.com"),
                pair("fetch.max_tags_per_repo", "7"),
                pair("hosts.ghe.corp.com.token_env", "GHE_TOKEN"),
                pair("policy_bump", "minor"),
                pair("_policy", "x"),
                pair("policy._bump", "x"),
            ]
        );
    }

    #[test]
    fn from_vars_skips_unprefixed_and_empty_level_names() {
        assert_eq!(
            keys(&[
                ("GITHUB_TOKEN", "t"),
                ("ACTUP", "x"),
                ("ACTUP_", "x"),
                ("ACTUP___POLICY", "x"),
                ("ACTUP_POLICY__", "x"),
                ("ACTUP_POLICY____BUMP", "x"),
                ("ACTUPé", "x"),
                ("", "x"),
                ("XACTUP_POLICY__BUMP", "x"),
            ]),
            Vec::<(String, String)>::new()
        );
    }

    #[test]
    fn from_vars_skips_cli_flag_variables() {
        assert_eq!(
            keys(&[
                ("ACTUP_CONFIG", "x.toml"),
                ("actup_config", "y.toml"),
                ("ACTUP_VERBOSE", "1"),
                ("ACTUP_FORMAT", "json"),
                ("Actup_Strict", "true"),
                ("ACTUP_COLOR", "never"),
                ("ACTUP_POLICY__BUMP", "patch"),
            ]),
            [pair("policy.bump", "patch")]
        );
    }

    #[test]
    fn data_nests_keys_and_later_values_win() -> Result<(), Box<dyn std::error::Error>> {
        let env = EnvOverrides::from_vars([
            ("ACTUP_POLICY__BUMP", "minor"),
            ("ACTUP_POLICY__PRERELEASE", "true"),
            ("actup_policy__bump", "patch"),
            ("ACTUP_FETCH__MAX_TAGS_PER_REPO", "7"),
            ("ACTUP_HOSTS__GHE.CORP.COM__KIND", "github"),
        ]);
        let value: Value = Figment::from(env).extract()?;
        let find = |path: &[&str]| {
            path.iter()
                .try_fold(&value, |value, key| value.as_dict()?.get(*key))
        };
        assert_eq!(
            find(&["policy", "bump"]).and_then(Value::as_str),
            Some("patch")
        );
        assert_eq!(
            find(&["policy", "prerelease"]).and_then(Value::to_bool),
            Some(true)
        );
        assert_eq!(
            find(&["fetch", "max_tags_per_repo"]).and_then(Value::to_u128),
            Some(7)
        );
        assert_eq!(
            find(&["hosts", "ghe.corp.com", "kind"]).and_then(Value::as_str),
            Some("github")
        );
        Ok(())
    }

    #[test]
    fn data_lets_a_later_leaf_replace_a_table_and_back() -> Result<(), Box<dyn std::error::Error>> {
        let env = EnvOverrides::from_vars([
            ("ACTUP_POLICY__BUMP", "minor"),
            ("ACTUP_POLICY", "flat"),
            ("ACTUP_CACHE", "flat"),
            ("ACTUP_CACHE__TTL", "60"),
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

    #[test]
    fn metadata_names_the_prefix_and_renders_the_variable_name() {
        let metadata = EnvOverrides::default().metadata();
        assert_eq!(metadata.name, "`ACTUP_` environment variable(s)");
        assert_eq!(
            metadata.interpolate(&Profile::Default, &["fetch", "max_tags_per_repo"]),
            "ACTUP_FETCH__MAX_TAGS_PER_REPO"
        );
    }
}
