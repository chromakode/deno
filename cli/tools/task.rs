// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use crate::args::CliOptions;
use crate::args::Flags;
use crate::args::TaskFlags;
use crate::colors;
use crate::factory::CliFactory;
use crate::npm::ManagedCliNpmResolver;
use crate::util::fs::canonicalize_path;
use deno_core::anyhow::bail;
use deno_core::anyhow::Context;
use deno_core::error::AnyError;
use deno_core::futures;
use deno_core::futures::future::LocalBoxFuture;
use deno_runtime::deno_node::NodeResolver;
use deno_semver::package::PackageNv;
use deno_task_shell::ExecuteResult;
use deno_task_shell::ShellCommand;
use deno_task_shell::ShellCommandContext;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use tokio::task::LocalSet;

pub async fn execute_script(
  flags: Flags,
  task_flags: TaskFlags,
) -> Result<i32, AnyError> {
  let factory = CliFactory::from_flags(flags).await?;
  let cli_options = factory.cli_options();
  let tasks_config = cli_options.resolve_tasks_config()?;
  let maybe_package_json = cli_options.maybe_package_json();
  let package_json_scripts = maybe_package_json
    .as_ref()
    .and_then(|p| p.scripts.clone())
    .unwrap_or_default();

  let task_name = match &task_flags.task {
    Some(task) => task,
    None => {
      print_available_tasks(&tasks_config, &package_json_scripts);
      return Ok(1);
    }
  };

  if let Some(script) = tasks_config.get(task_name) {
    let config_file_url = cli_options.maybe_config_file_specifier().unwrap();
    let config_file_path = if config_file_url.scheme() == "file" {
      config_file_url.to_file_path().unwrap()
    } else {
      bail!("Only local configuration files are supported")
    };
    let cwd = match task_flags.cwd {
      Some(path) => canonicalize_path(&PathBuf::from(path))?,
      None => config_file_path.parent().unwrap().to_owned(),
    };
    let script = get_script_with_args(script, cli_options);
    output_task(task_name, &script);
    let seq_list = deno_task_shell::parser::parse(&script)
      .with_context(|| format!("Error parsing script '{task_name}'."))?;
    let env_vars = collect_env_vars();
    let local = LocalSet::new();
    let future =
      deno_task_shell::execute(seq_list, env_vars, &cwd, Default::default());
    let exit_code = local.run_until(future).await;
    Ok(exit_code)
  } else if package_json_scripts.contains_key(task_name) {
    let package_json_deps_provider = factory.package_json_deps_provider();
    let npm_resolver = factory.npm_resolver().await?;
    let node_resolver = factory.node_resolver().await?;

    if let Some(package_deps) = package_json_deps_provider.deps() {
      for (key, value) in package_deps {
        if let Err(err) = value {
          log::info!(
            "{} Ignoring dependency '{}' in package.json because its version requirement failed to parse: {:#}",
            colors::yellow("Warning"),
            key,
            err,
          );
        }
      }
    }

    // install the npm packages if we're using a managed resolver
    if let Some(npm_resolver) = npm_resolver.as_managed() {
      npm_resolver.ensure_top_level_package_json_install().await?;
      npm_resolver.resolve_pending().await?;
    }

    let cwd = match task_flags.cwd {
      Some(path) => canonicalize_path(&PathBuf::from(path))?,
      None => maybe_package_json
        .as_ref()
        .unwrap()
        .path
        .parent()
        .unwrap()
        .to_owned(),
    };

    // At this point we already checked if the task name exists in package.json.
    // We can therefore check for "pre" and "post" scripts too, since we're only
    // dealing with package.json here and not deno.json
    let task_names = vec![
      format!("pre{}", task_name),
      task_name.clone(),
      format!("post{}", task_name),
    ];
    for task_name in task_names {
      if let Some(script) = package_json_scripts.get(&task_name) {
        let script = get_script_with_args(script, cli_options);
        output_task(&task_name, &script);
        let seq_list = deno_task_shell::parser::parse(&script)
          .with_context(|| format!("Error parsing script '{task_name}'."))?;
        let npx_commands = match npm_resolver.as_managed() {
          Some(npm_resolver) => {
            resolve_npm_commands(npm_resolver, node_resolver)?
          }
          None => Default::default(),
        };
        let env_vars = match npm_resolver.root_node_modules_path() {
          Some(dir_path) => collect_env_vars_with_node_modules_dir(dir_path),
          None => collect_env_vars(),
        };
        let local = LocalSet::new();
        let future =
          deno_task_shell::execute(seq_list, env_vars, &cwd, npx_commands);
        let exit_code = local.run_until(future).await;
        if exit_code > 0 {
          return Ok(exit_code);
        }
      }
    }

    Ok(0)
  } else {
    eprintln!("Task not found: {task_name}");
    print_available_tasks(&tasks_config, &package_json_scripts);
    Ok(1)
  }
}

fn get_script_with_args(script: &str, options: &CliOptions) -> String {
  let additional_args = options
    .argv()
    .iter()
    // surround all the additional arguments in double quotes
    // and sanitize any command substitution
    .map(|a| format!("\"{}\"", a.replace('"', "\\\"").replace('$', "\\$")))
    .collect::<Vec<_>>()
    .join(" ");
  let script = format!("{script} {additional_args}");
  script.trim().to_owned()
}

fn output_task(task_name: &str, script: &str) {
  log::info!(
    "{} {} {}",
    colors::green("Task"),
    colors::cyan(&task_name),
    script,
  );
}

fn collect_env_vars_with_node_modules_dir(
  node_modules_dir_path: &Path,
) -> HashMap<String, String> {
  let mut env_vars = collect_env_vars();
  prepend_to_path(
    &mut env_vars,
    node_modules_dir_path
      .join(".bin")
      .to_string_lossy()
      .to_string(),
  );
  env_vars
}

fn prepend_to_path(env_vars: &mut HashMap<String, String>, value: String) {
  match env_vars.get_mut("PATH") {
    Some(path) => {
      if path.is_empty() {
        *path = value;
      } else {
        *path =
          format!("{}{}{}", value, if cfg!(windows) { ";" } else { ":" }, path);
      }
    }
    None => {
      env_vars.insert("PATH".to_string(), value);
    }
  }
}

fn collect_env_vars() -> HashMap<String, String> {
  // get the starting env vars (the PWD env var will be set by deno_task_shell)
  let mut env_vars = std::env::vars().collect::<HashMap<String, String>>();
  const INIT_CWD_NAME: &str = "INIT_CWD";
  if !env_vars.contains_key(INIT_CWD_NAME) {
    if let Ok(cwd) = std::env::current_dir() {
      // if not set, set an INIT_CWD env var that has the cwd
      env_vars
        .insert(INIT_CWD_NAME.to_string(), cwd.to_string_lossy().to_string());
    }
  }
  env_vars
}

fn print_available_tasks(
  // order can be important, so these use an index map
  tasks_config: &IndexMap<String, String>,
  package_json_scripts: &IndexMap<String, String>,
) {
  eprintln!("{}", colors::green("Available tasks:"));

  let mut had_task = false;
  for (is_deno, (key, value)) in tasks_config.iter().map(|e| (true, e)).chain(
    package_json_scripts
      .iter()
      .filter(|(key, _)| !tasks_config.contains_key(*key))
      .map(|e| (false, e)),
  ) {
    eprintln!(
      "- {}{}",
      colors::cyan(key),
      if is_deno {
        "".to_string()
      } else {
        format!(" {}", colors::italic_gray("(package.json)"))
      }
    );
    eprintln!("    {value}");
    had_task = true;
  }
  if !had_task {
    eprintln!("  {}", colors::red("No tasks found in configuration file"));
  }
}

struct NpxCommand;

impl ShellCommand for NpxCommand {
  fn execute(
    &self,
    mut context: ShellCommandContext,
  ) -> LocalBoxFuture<'static, ExecuteResult> {
    if let Some(first_arg) = context.args.get(0).cloned() {
      if let Some(command) = context.state.resolve_command(&first_arg) {
        let context = ShellCommandContext {
          args: context.args.iter().skip(1).cloned().collect::<Vec<_>>(),
          ..context
        };
        command.execute(context)
      } else {
        let _ = context
          .stderr
          .write_line(&format!("npx: could not resolve command '{first_arg}'"));
        Box::pin(futures::future::ready(ExecuteResult::from_exit_code(1)))
      }
    } else {
      let _ = context.stderr.write_line("npx: missing command");
      Box::pin(futures::future::ready(ExecuteResult::from_exit_code(1)))
    }
  }
}

#[derive(Clone)]
struct NpmPackageBinCommand {
  name: String,
  npm_package: PackageNv,
}

impl ShellCommand for NpmPackageBinCommand {
  fn execute(
    &self,
    context: ShellCommandContext,
  ) -> LocalBoxFuture<'static, ExecuteResult> {
    let mut args = vec![
      "run".to_string(),
      "-A".to_string(),
      if self.npm_package.name == self.name {
        format!("npm:{}", self.npm_package)
      } else {
        format!("npm:{}/{}", self.npm_package, self.name)
      },
    ];
    args.extend(context.args);
    let executable_command =
      deno_task_shell::ExecutableCommand::new("deno".to_string());
    executable_command.execute(ShellCommandContext { args, ..context })
  }
}

fn resolve_npm_commands(
  npm_resolver: &ManagedCliNpmResolver,
  node_resolver: &NodeResolver,
) -> Result<HashMap<String, Rc<dyn ShellCommand>>, AnyError> {
  let mut result = HashMap::new();
  let snapshot = npm_resolver.snapshot();
  for id in snapshot.top_level_packages() {
    let package_folder = npm_resolver.resolve_pkg_folder_from_pkg_id(id)?;
    let bin_commands =
      node_resolver.resolve_binary_commands(&package_folder)?;
    for bin_command in bin_commands {
      result.insert(
        bin_command.to_string(),
        Rc::new(NpmPackageBinCommand {
          name: bin_command,
          npm_package: id.nv.clone(),
        }) as Rc<dyn ShellCommand>,
      );
    }
  }
  if !result.contains_key("npx") {
    result.insert("npx".to_string(), Rc::new(NpxCommand));
  }
  Ok(result)
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn test_prepend_to_path() {
    let mut env_vars = HashMap::new();

    prepend_to_path(&mut env_vars, "/example".to_string());
    assert_eq!(
      env_vars,
      HashMap::from([("PATH".to_string(), "/example".to_string())])
    );

    prepend_to_path(&mut env_vars, "/example2".to_string());
    let separator = if cfg!(windows) { ";" } else { ":" };
    assert_eq!(
      env_vars,
      HashMap::from([(
        "PATH".to_string(),
        format!("/example2{}/example", separator)
      )])
    );

    env_vars.get_mut("PATH").unwrap().clear();
    prepend_to_path(&mut env_vars, "/example".to_string());
    assert_eq!(
      env_vars,
      HashMap::from([("PATH".to_string(), "/example".to_string())])
    );
  }
}
