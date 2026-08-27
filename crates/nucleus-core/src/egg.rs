use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EggVariable {
    pub name: String,
    pub env_variable: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default_value: String,
    #[serde(default = "default_true")]
    pub user_editable: bool,
    #[serde(default)]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Egg {
    pub slug: String,
    pub name: String,
    pub docker_images: Vec<String>,
    pub startup: String,
    #[serde(default)]
    pub variables: Vec<EggVariable>,
    #[serde(default)]
    pub stop_command: Option<String>,
    #[serde(default)]
    pub install_script: Option<String>,
    /// Image used to run the install script (defaults to the first docker image).
    #[serde(default)]
    pub installer_image: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PteroEggAttr {
    name: String,
    #[serde(default)]
    docker_images: serde_json::Value,
    startup: String,
    #[serde(default)]
    variables: Vec<PteroVar>,
    #[serde(default)]
    scripts: Option<PteroScripts>,
    #[serde(default)]
    installer_image: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PteroVar {
    name: String,
    env_variable: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    default_value: Option<serde_json::Value>,
    #[serde(default)]
    user_editable: Option<bool>,
    #[serde(default)]
    required: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PteroScripts {
    installation: Option<PteroInstall>,
}

#[derive(Debug, Deserialize)]
struct PteroInstall {
    #[serde(default)]
    script: Option<String>,
}

fn value_to_string(v: &Option<serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

pub fn import_ptero_egg(json: &str) -> Result<Egg> {
    let v: serde_json::Value =
        serde_json::from_str(json).context("not a valid Pterodactyl egg JSON")?;
    // Accept both the Pterodactyl application-API envelope ({attributes:{…}})
    // and the flat layout used by egg files in community repos (game-eggs).
    let attrs = match (v.get("attributes"), v.get("name"), v.get("startup")) {
        (Some(_), _, _) => &v["attributes"],
        (_, Some(_), Some(_)) => &v,
        _ => return Err(anyhow!("not a valid Pterodactyl egg JSON")),
    };
    let a: PteroEggAttr = serde_json::from_value(attrs.clone())
        .context("invalid egg fields")?;

    let images: Vec<String> = match a.docker_images {
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .filter(|s| !s.is_empty())
            .collect(),
        serde_json::Value::Object(map) => map
            .into_values()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .filter(|s| !s.is_empty())
            .collect(),
        _ => vec![],
    };
    if images.is_empty() {
        return Err(anyhow!("egg has no docker images"));
    }

    let variables = a
        .variables
        .into_iter()
        .map(|v| EggVariable {
            name: v.name,
            env_variable: v.env_variable,
            description: v.description,
            default_value: value_to_string(&v.default_value),
            user_editable: v.user_editable.unwrap_or(true),
            required: v.required.unwrap_or(false),
        })
        .collect();

    Ok(Egg {
        slug: slugify(&a.name),
        name: a.name,
        docker_images: images,
        startup: a.startup,
        variables,
        stop_command: None,
        install_script: a
            .scripts
            .and_then(|s| s.installation)
            .and_then(|i| i.script),
        installer_image: a.installer_image,
    })
}

pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if (c == ' ' || c == '-' || c == '_') && !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Substitute `{{VAR}}` placeholders in an egg startup command using resolved values.
pub fn render_startup(template: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes: Vec<char> = template.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '{' && i + 1 < bytes.len() && bytes[i + 1] == '{' {
            if let Some(end_rel) = find_close(&bytes[i + 2..]) {
                let key: String = bytes[i + 2..i + 2 + end_rel].iter().collect();
                let key_trim = key.trim();
                if is_var_name(key_trim) {
                    out.push_str(values.get(key_trim).map(String::as_str).unwrap_or(""));
                    i += 2 + end_rel + 2;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn find_close(chars: &[char]) -> Option<usize> {
    chars.windows(2).position(|w| w[0] == '}' && w[1] == '}')
}

fn is_var_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':' || c == '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_EGG: &str = r#"{
        "attributes": {
            "name": "Vanilla Minecraft",
            "docker_images": {"Java 17": "ghcr.io/pterodactyl/yolks:java_17"},
            "startup": "java -Xms128M -Xmx{{SERVER_MEMORY}}M -jar {{SERVER_JARFILE}}",
            "variables": [
                {"name": "Server Jar", "env_variable": "SERVER_JARFILE", "default_value": "server.jar", "user_editable": true, "required": true}
            ]
        }
    }"#;

    #[test]
    fn imports_ptero_egg() {
        let egg = import_ptero_egg(SAMPLE_EGG).unwrap();
        assert_eq!(egg.slug, "vanilla-minecraft");
        assert_eq!(egg.docker_images[0], "ghcr.io/pterodactyl/yolks:java_17");
        assert_eq!(egg.variables[0].env_variable, "SERVER_JARFILE");
    }

    #[test]
    fn rejects_non_egg() {
        assert!(import_ptero_egg("{}").is_err());
    }

    #[test]
    fn renders_placeholders() {
        let mut vals = BTreeMap::new();
        vals.insert("SERVER_MEMORY".to_string(), "2048".to_string());
        vals.insert("SERVER_JARFILE".to_string(), "server.jar".to_string());
        let cmd = render_startup("java -Xmx{{SERVER_MEMORY}}M -jar {{SERVER_JARFILE}}", &vals);
        assert_eq!(cmd, "java -Xmx2048M -jar server.jar");
    }

    #[test]
    fn unknown_placeholder_becomes_empty() {
        let cmd = render_startup("x {{NOPE}} y", &Default::default());
        assert_eq!(cmd, "x  y");
    }

    #[test]
    fn leaves_shell_braces_alone() {
        let cmd = render_startup("sh -c 'echo ${HOME}'", &Default::default());
        assert_eq!(cmd, "sh -c 'echo ${HOME}'");
    }
}
