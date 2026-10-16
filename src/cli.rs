//! Command-line front end. `run` is dispatched by `main` because it is async.
use crate::admission::{mutate, validate};
use crate::contract;
use crate::crd::{crd_yaml, Website};
use crate::reconcile::{observe, plan, ObservedDeployment, FINALIZER};
use crate::Error;
use std::io::Write;

const USAGE: &str = "usage: opctl <command>
  crd                         print the Website CustomResourceDefinition as YAML
  plan <file> [--deployed IMAGE REPLICAS READY] [--deleting]
                              print the actions a reconcile would take
  validate <review.json>      run the validating webhook on an AdmissionReview
  mutate <review.json>        run the mutating webhook on an AdmissionReview
  contract                    check the admission invariants on a matrix of inputs
  run [--namespace NS]        run the controller against the current kubeconfig
exit codes: 0 ok or allowed, 1 denied, 2 usage or input error";

fn read(path: &str) -> Result<String, Error> {
    std::fs::read_to_string(path).map_err(|e| Error::Parse(format!("{path}: {e}")))
}

fn emit(out: &mut dyn Write, text: &str) -> Result<(), Error> {
    writeln!(out, "{}", text.trim_end())
        .map_err(|e| Error::Parse(format!("cannot write output: {e}")))
}

/// Parses `run` arguments into an optional namespace.
pub fn parse_run_args(args: &[String]) -> Result<Option<String>, Error> {
    match args {
        [] => Ok(None),
        [flag, ns] if flag == "--namespace" => Ok(Some(ns.clone())),
        _ => Err(Error::Parse("run takes only [--namespace NS]".into())),
    }
}

fn number(s: &str) -> Result<i32, Error> {
    s.parse::<i32>()
        .map_err(|_| Error::Parse(format!("not a number: {s}")))
}

fn plan_cmd(args: &[String], out: &mut dyn Write) -> Result<i32, Error> {
    let (file, flags) = args
        .split_first()
        .ok_or_else(|| Error::Parse("plan needs a file".into()))?;
    let mut site: Website =
        serde_yaml::from_str(&read(file)?).map_err(|e| Error::Parse(format!("{file}: {e}")))?;
    let mut deployment = None;
    let mut i = 0;
    while i < flags.len() {
        match flags[i].as_str() {
            "--deleting" => {
                site.metadata.deletion_timestamp =
                    Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                        k8s_openapi::jiff::Timestamp::UNIX_EPOCH,
                    ));
                site.metadata.finalizers = Some(vec![FINALIZER.to_string()]);
                i += 1;
            }
            "--deployed" if i + 3 < flags.len() => {
                deployment = Some(ObservedDeployment {
                    image: flags[i + 1].clone(),
                    replicas: number(&flags[i + 2])?,
                    ready_replicas: number(&flags[i + 3])?,
                });
                i += 4;
            }
            other => return Err(Error::Parse(format!("unexpected argument: {other}"))),
        }
    }
    let actions = plan(&site.spec, &observe(&site, deployment));
    let text = serde_json::to_string_pretty(&actions).expect("actions serialise to JSON");
    emit(out, &text)?;
    Ok(0)
}

fn review_cmd(
    args: &[String],
    out: &mut dyn Write,
    f: fn(&str) -> Result<(String, bool), Error>,
) -> Result<i32, Error> {
    let file = args
        .first()
        .ok_or_else(|| Error::Parse("a review file is required".into()))?;
    let (text, allowed) = f(&read(file)?)?;
    emit(out, &text)?;
    Ok(if allowed { 0 } else { 1 })
}

/// Runs every command except `run`. Returns the process exit code.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let result = match args.split_first() {
        Some((cmd, rest)) if cmd == "crd" && rest.is_empty() => emit(out, &crd_yaml()).map(|_| 0),
        Some((cmd, rest)) if cmd == "contract" && rest.is_empty() => contract::check()
            .and_then(|_| emit(out, "contract: admission-refuses-unpinned-or-oversized OK"))
            .map(|_| 0),
        Some((cmd, rest)) if cmd == "plan" => plan_cmd(rest, out),
        Some((cmd, rest)) if cmd == "validate" => review_cmd(rest, out, validate),
        Some((cmd, rest)) if cmd == "mutate" => review_cmd(rest, out, mutate),
        Some((cmd, _)) if cmd == "help" || cmd == "--help" => emit(out, USAGE).map(|_| 0),
        _ => Err(Error::Parse(USAGE.to_string())),
    };
    result.unwrap_or_else(|e| {
        let _ = writeln!(err, "opctl: {e}");
        2
    })
}
