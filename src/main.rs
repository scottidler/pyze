#![cfg_attr(debug_assertions, allow(unused_imports, unused_variables, unused_mut, dead_code))]

// Standard library imports
use std::path::{Path, PathBuf};
use std::process::Command;

// External crates
use clap::Parser;
use eyre::WrapErr;
use eyre::{eyre, Result};
use reqwest;
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser, Debug)]
#[clap(name = "dock", about = "Dockerize any Python script")]
struct Cli {
    #[clap(required = true, help = "Python Script")]
    script: PathBuf,

    #[clap(help = "Optional list of args")]
    args: Vec<String>,
}

#[derive(Debug)]
enum PythonImport {
    ModuleOnly(String),
    ModuleWithMember(String, String),
}

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    defaults: Option<Defaults>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Defaults {
    #[serde(rename = "import-mappings")]
    import_mappings: Option<std::collections::HashMap<String, String>>,
}

async fn load_config() -> Result<Config> {
    let config_path = dirs::home_dir().unwrap().join(".config/pyze/pyze.yml");
    if config_path.exists() {
        let mut file = tokio::fs::File::open(&config_path).await?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).await?;
        let config: Config = serde_yaml::from_str(&contents)?;
        Ok(config)
    } else {
        Ok(Config { defaults: None })
    }
}

fn remap_modules(modules: &[String], mappings: &Option<std::collections::HashMap<String, String>>) -> Vec<String> {
    modules
        .iter()
        .map(|module| {
            mappings
                .as_ref()
                .and_then(|map| map.get(module))
                .cloned()
                .unwrap_or_else(|| module.clone())
        })
        .collect()
}

async fn parse_python_file(script: &PathBuf) -> Result<Vec<PythonImport>> {
    let mut file = File::open(script).await?;
    let mut content = String::new();
    file.read_to_string(&mut content).await?;

    let imports: Vec<PythonImport> = content
        .lines()
        .filter_map(|line| {
            let trimmed_line = line.trim();
            if trimmed_line.starts_with("import ") {
                Some(PythonImport::ModuleOnly(trimmed_line[7..].trim().to_string()))
            } else if trimmed_line.starts_with("from ") {
                let parts: Vec<&str> = trimmed_line[5..].split(" import ").collect();
                if parts.len() == 2 {
                    Some(PythonImport::ModuleWithMember(
                        parts[0].trim().to_string(),
                        parts[1].trim().to_string(),
                    ))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    Ok(imports)
}

fn get_python_builtins_stdlibs() -> Result<Vec<String>> {
    // Python code as a Rust string
    let python_code = r#"
import sys

# Get built-in modules
builtin_modules = set(sys.builtin_module_names)

# Get standard library modules
standard_lib_modules = set(sys.stdlib_module_names)

# Combine both
all_default_modules = builtin_modules.union(standard_lib_modules)

# Assuming all_default_modules is your original set of modules
filtered_modules = {module for module in all_default_modules if not module.startswith('_')}

for module in sorted(filtered_modules):
    print(module)
"#;

    // Execute the Python code and capture the output
    let output = Command::new("python3")
        .arg("-c")
        .arg(python_code)
        .output()
        .expect("Failed to execute command");

    if !output.status.success() {
        return Err(eyre::eyre!(
            "Command execution failed with error: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let output_str = std::str::from_utf8(&output.stdout)?;
    Ok(output_str.lines().map(|s| s.to_string()).collect())
}

/*
async fn check_package_exists(package: &str) -> bool {
    let url = format!("https://pypi.org/pypi/{}/json", package);
    match reqwest::get(&url).await {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}
*/

async fn check_package_exists(package: &str) -> Option<String> {
    // First, try the full package name
    let url = format!("https://pypi.org/pypi/{}/json", package);
    if let Ok(resp) = reqwest::get(&url).await {
        if resp.status().is_success() {
            return Some(package.to_string());
        }
    }

    // Fallback to the root package name
    let root_package: &str = package.split('.').next().unwrap();
    let url = format!("https://pypi.org/pypi/{}/json", root_package);
    if let Ok(resp) = reqwest::get(&url).await {
        if resp.status().is_success() {
            return Some(root_package.to_string());
        }
    }

    None
}

async fn generate_dockerfile(
    python_version: &str,
    modules: &[String],
    script_name: &str,
    output_dir: &Path,
) -> Result<()> {
    // Read the Dockerfile.template into a String
    let default_template = r#"
FROM python:{{PYTHON_VERSION}}

RUN useradd -ms /bin/bash dock
USER dock

RUN pip install {{MODULES}}

COPY {{SCRIPT_NAME}} /home/dock/{{SCRIPT_NAME}}
WORKDIR /home/dock

ENTRYPOINT ["python3", "{{SCRIPT_NAME}}"]
"#;

    let template = std::env::var("DOCKERFILE_TEMPLATE")
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_else(|| default_template.to_string());

    // Replace placeholders with actual values
    let filled_template = template
        .replace("{{PYTHON_VERSION}}", python_version)
        .replace("{{MODULES}}", &modules.join(" "))
        .replace("{{SCRIPT_NAME}}", script_name);

    // Write the filled template to the output Dockerfile
    let dockerfile_path = output_dir.join("Dockerfile");
    let mut output_file = tokio::fs::File::create(&dockerfile_path).await?;
    output_file.write_all(filled_template.as_bytes()).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = load_config().await?;
    let cli: Cli = Cli::parse();
    let builtin_stdlibs = get_python_builtins_stdlibs()?;
    let imports = parse_python_file(&cli.script).await?;
    let mut modules = Vec::new();

    for import in imports {
        match import {
            PythonImport::ModuleOnly(module) => {
                if !builtin_stdlibs.contains(&module) {
                    if let Some(valid_module) = check_package_exists(&module).await {
                        modules.push(valid_module);
                    }
                }
            }
            PythonImport::ModuleWithMember(module, object) => {
                if !builtin_stdlibs.contains(&module) {
                    let full_name = format!("{}.{}", &module, &object);
                    if let Some(valid_module) = check_package_exists(&full_name).await {
                        modules.push(valid_module);
                    } else if let Some(valid_module) = check_package_exists(&module).await {
                        modules.push(valid_module);
                    }
                }
            }
        }
    }

    // Remove duplicates
    modules.sort();
    modules.dedup();

    // Remap modules using the import_mappings from the config
    let remapped_modules = remap_modules(&modules, &config.defaults.and_then(|d| d.import_mappings));
    let python_version = "3.10";
    let script_name = cli
        .script
        .file_name()
        .ok_or(eyre!("Failed to get file name"))?
        .to_str()
        .ok_or(eyre!("Failed to convert to str"))?;

    let script_path = cli.script.parent().ok_or(eyre!("Failed to get parent directory"))?;

    generate_dockerfile(python_version, &remapped_modules, script_name, &script_path).await?;

    Command::new("docker")
        .env("DOCKER_BUILDKIT", "1")
        .args(&[
            "build",
            "-t",
            script_name,
            script_path.to_str().ok_or(eyre!("Failed to convert path to str"))?,
        ])
        .status()
        .wrap_err("Failed to build Docker image")?;

    Command::new("docker")
        .args(&["run", script_name])
        .args(&cli.args)
        .status()
        .wrap_err("Failed to run Docker container")?;

    Ok(())
}
