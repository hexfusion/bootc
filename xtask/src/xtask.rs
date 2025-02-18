use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Cursor, Write};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use fn_error_context::context;
use xshell::{cmd, Shell};

const NAME: &str = "bootc";
const VENDORPATH: &str = "target/vendor.tar.zstd";

fn main() {
    if let Err(e) = try_main() {
        eprintln!("error: {e:?}");
        std::process::exit(1);
    }
}

#[allow(clippy::type_complexity)]
const TASKS: &[(&str, fn(&Shell) -> Result<()>)] = &[
    ("vendor", vendor),
    ("manpages", manpages),
    ("man2markdown", man2markdown),
    ("package", package),
    ("package-srpm", package_srpm),
    ("custom-lints", custom_lints),
];

fn try_main() -> Result<()> {
    let task = std::env::args().nth(1);
    let sh = xshell::Shell::new()?;
    if let Some(cmd) = task.as_deref() {
        let f = TASKS
            .iter()
            .find_map(|(k, f)| (*k == cmd).then_some(*f))
            .unwrap_or(print_help);
        f(&sh)?;
    } else {
        print_help(&sh)?;
    }
    Ok(())
}

fn vendor(sh: &Shell) -> Result<()> {
    let target = VENDORPATH;
    cmd!(
        sh,
        "cargo vendor-filterer --prefix=vendor --format=tar.zstd {target}"
    )
    .run()?;
    Ok(())
}

fn gitrev_to_version(v: &str) -> String {
    let v = v.trim().trim_start_matches('v');
    v.replace('-', ".")
}

#[context("Finding gitrev")]
fn gitrev(sh: &Shell) -> Result<String> {
    if let Ok(rev) = cmd!(sh, "git describe --tags --exact-match")
        .ignore_stderr()
        .read()
    {
        Ok(gitrev_to_version(&rev))
    } else {
        // Grab the abbreviated commit
        let abbrev_commit = cmd!(sh, "git rev-parse HEAD")
            .read()?
            .chars()
            .take(10)
            .collect::<String>();
        let timestamp = git_timestamp(sh)?;
        // We always inject the timestamp first to ensure that newer is better.
        Ok(format!("{timestamp}.g{abbrev_commit}"))
    }
}

#[context("Manpages")]
fn manpages(sh: &Shell) -> Result<()> {
    sh.create_dir("target/man")?;
    cmd!(
        sh,
        "cargo run --features=docgen -- man --directory target/man"
    )
    .run()?;
    Ok(())
}

/// Generate markdown files (converted from the man pages, which are generated
/// from the Rust sources) into docs/man.  This process is currently manual.
#[context("man2markdown")]
fn man2markdown(sh: &Shell) -> Result<()> {
    manpages(sh)?;
    for ent in std::fs::read_dir("target/man")? {
        let ent = ent?;
        let path = &ent.path();
        if path.extension().and_then(|s| s.to_str()) != Some("8") {
            continue;
        }
        let filename = path
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("Expected filename in {path:?}"))?;
        let target = format!("manpages-md/{filename}.md");
        cmd!(
            sh,
            "pandoc --from=man --to=markdown --output={target} {path}"
        )
        .run()?;
    }
    Ok(())
}

/// Return a string formatted version of the git commit timestamp, up to the minute
/// but not second because, well, we're not going to build more than once a second.
#[context("Finding git timestamp")]
fn git_timestamp(sh: &Shell) -> Result<String> {
    let ts = cmd!(sh, "git show -s --format=%ct").read()?;
    let ts = ts.trim().parse::<i64>()?;
    let ts = chrono::NaiveDateTime::from_timestamp_opt(ts, 0)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse timestamp"))?;
    Ok(ts.format("%Y%m%d%H%M").to_string())
}

struct Package {
    version: String,
    srcpath: Utf8PathBuf,
}

/// Return the timestamp of the latest git commit in seconds since the Unix epoch.
fn git_source_date_epoch(dir: &Utf8Path) -> Result<u64> {
    let o = Command::new("git")
        .args(["log", "-1", "--pretty=%ct"])
        .current_dir(dir)
        .output()?;
    if !o.status.success() {
        anyhow::bail!("git exited with an error: {:?}", o);
    }
    let buf = String::from_utf8(o.stdout).context("Failed to parse git log output")?;
    let r = buf.trim().parse()?;
    Ok(r)
}

#[context("Packaging")]
fn impl_package(sh: &Shell) -> Result<Package> {
    let source_date_epoch = git_source_date_epoch(".".into())?;
    manpages(sh)?;
    let v = gitrev(sh)?;

    let namev = format!("{NAME}-{v}");
    let p = Utf8Path::new("target").join(format!("{namev}.tar"));
    let o = File::create(&p)?;
    let prefix = format!("{namev}/");
    let st = Command::new("git")
        .args([
            "archive",
            "--format=tar",
            "--prefix",
            prefix.as_str(),
            "HEAD",
        ])
        .stdout(Stdio::from(o))
        .status()?;
    if !st.success() {
        anyhow::bail!("Failed to run {st:?}");
    }
    let st = Command::new("tar")
        .args([
            "-r",
            "-C",
            "target",
            "--sort=name",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime",
        ])
        .arg(format!("--transform=s,^,{prefix},"))
        .arg(format!("--mtime=@{source_date_epoch}"))
        .args(["-f", p.as_str(), "man"])
        .status()
        .context("Failed to execute tar")?;
    if !st.success() {
        anyhow::bail!("Failed to run {st:?}");
    }
    cmd!(sh, "zstd -f {p}").run()?;
    Ok(Package {
        version: v,
        srcpath: format!("{p}.zst").into(),
    })
}

fn package(sh: &Shell) -> Result<()> {
    let p = impl_package(sh)?.srcpath;
    println!("Generated: {p}");
    Ok(())
}

fn impl_srpm(sh: &Shell) -> Result<Utf8PathBuf> {
    {
        let _g = sh.push_dir("target");
        for name in sh.read_dir(".")? {
            if let Some(name) = name.to_str() {
                if name.ends_with(".src.rpm") {
                    sh.remove_path(name)?;
                }
            }
        }
    }
    let pkg = impl_package(sh)?;
    vendor(sh)?;
    let td = tempfile::tempdir_in("target").context("Allocating tmpdir")?;
    let td = td.into_path();
    let td: &Utf8Path = td.as_path().try_into().unwrap();
    let srcpath = td.join(pkg.srcpath.file_name().unwrap());
    std::fs::rename(pkg.srcpath, srcpath)?;
    let v = pkg.version;
    let vendorpath = td.join(format!("{NAME}-{v}-vendor.tar.zst"));
    std::fs::rename(VENDORPATH, vendorpath)?;
    {
        let specin = File::open(format!("contrib/packaging/{NAME}.spec"))
            .map(BufReader::new)
            .context("Opening spec")?;
        let mut o = File::create(td.join(format!("{NAME}.spec"))).map(BufWriter::new)?;
        for line in specin.lines() {
            let line = line?;
            if line.starts_with("Version:") {
                writeln!(o, "# Replaced by cargo xtask package-srpm")?;
                writeln!(o, "Version: {v}")?;
            } else {
                writeln!(o, "{}", line)?;
            }
        }
    }
    let d = sh.push_dir(td);
    let mut cmd = cmd!(sh, "rpmbuild");
    for k in [
        "_sourcedir",
        "_specdir",
        "_builddir",
        "_srcrpmdir",
        "_rpmdir",
    ] {
        cmd = cmd.arg("--define");
        cmd = cmd.arg(format!("{k} {td}"));
    }
    cmd.arg("--define")
        .arg(format!("_buildrootdir {td}/.build"))
        .args(["-bs", "bootc.spec"])
        .run()?;
    drop(d);
    let mut srpm = None;
    for e in std::fs::read_dir(td)? {
        let e = e?;
        let n = e.file_name();
        let n = if let Some(n) = n.to_str() {
            n
        } else {
            continue;
        };
        if n.ends_with(".src.rpm") {
            srpm = Some(td.join(n));
            break;
        }
    }
    let srpm = srpm.ok_or_else(|| anyhow::anyhow!("Failed to find generated .src.rpm"))?;
    let dest = Utf8Path::new("target").join(srpm.file_name().unwrap());
    std::fs::rename(&srpm, &dest)?;
    Ok(dest)
}

fn package_srpm(sh: &Shell) -> Result<()> {
    let srpm = impl_srpm(sh)?;
    println!("Generated: {srpm}");
    Ok(())
}

fn custom_lints(sh: &Shell) -> Result<()> {
    // Verify there are no invocations of the dbg macro.
    let o = cmd!(sh, "git grep dbg\x21 *.rs").ignore_status().read()?;
    if !o.is_empty() {
        let mut stderr = std::io::stderr().lock();
        std::io::copy(&mut Cursor::new(o.as_bytes()), &mut stderr)?;
        eprintln!();
        anyhow::bail!("Found dbg\x21 macro");
    }
    Ok(())
}

fn print_help(_sh: &Shell) -> Result<()> {
    println!("Tasks:");
    for (name, _) in TASKS {
        println!("  - {name}");
    }
    Ok(())
}
