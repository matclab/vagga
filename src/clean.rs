use std::os;
use std::io::FileNotFound;
use std::io::stdio::{stdout, stderr};
use std::io::fs::{rmdir_recursive};
use std::io::fs::{readdir, readlink};
use std::io::process::Process;
use std::from_str::from_str;
use std::str::from_utf8;

use argparse::{ArgumentParser, StoreConst, List};
use collections::treemap::{TreeMap, TreeSet};

use super::env::Environ;
use super::monitor::Monitor;
use super::options::env_options;
use super::linux::{CPipe, run_newuser};
use super::uidmap::write_max_map;


enum CleanMode {
    Help,
    Container,
    TmpFolders,
    OldContainers,
    Transient,
    AllTransient,
    Everything,
}

pub fn run_do_rm(_env: &mut Environ, args: Vec<String>) -> Result<int, String>
{
    let mut dirs: Vec<String> = Vec::new();
    {
        let mut ap = ArgumentParser::new();
        ap.refer(&mut dirs)
            .add_argument("dirs", box List::<String>,
                "Names of directories to remove");
        match ap.parse(args, &mut stdout(), &mut stderr()) {
            Ok(()) => {}
            Err(0) => return Ok(0),
            Err(_) => return Ok(122),
        }
    }
    for d in dirs.iter() {
        try!(rmdir_recursive(&Path::new(d.as_slice()))
            .map_err(|e| format!("Error removing {}: {}", d, e)));
    }
    return Ok(0);
}

pub fn run_rmdirs(exe: &Path, dirs: Vec<Path>) -> Result<(), String> {
    let pipe = try!(CPipe::new());
    let mut monitor = Monitor::new(true);

    let mut args = vec!("__rm".to_string());
    args.extend(dirs.iter().map(|p| p.as_str().unwrap().to_string()));
    let pid = try!(run_newuser(&pipe,
        &exe.as_str().unwrap().to_string(),
        args.as_slice(),
        &TreeMap::new()));

    try!(write_max_map(pid));

    try!(pipe.wakeup());

    monitor.add("child".to_string(), pid);
    monitor.wait_all();
    if monitor.get_status() == 0 {
        return Ok(());
    }
    return Err(format!("Error removing dirs"));
}

pub fn run_clean(env: &mut Environ, args: Vec<String>) -> Result<int, String>
{
    let mut mode: CleanMode = Help;
    let mut names: Vec<String> = Vec::new();
    {
        let mut ap = ArgumentParser::new();
        ap.refer(&mut names)
            .add_argument("names", box List::<String>,
                "Names of containers to delete");
        ap.refer(&mut mode)
            .add_option(["--container"], box StoreConst(Container),
                "Delete specified container(s)")
            .add_option(["--tmp", "--tmp-folders"],
                box StoreConst(TmpFolders),
                "Delete .tmp folders. They are just garbage from previous
                 unsuccessful commands and safe to delete if you don't run any
                 other vagga processes in parallel")
            .add_option(["--everything"], box StoreConst(Everything),
                "Remove `.vagga` directory. Useful mostly before removing
                 project itself. Needed when subuid/subgid used, so some files
                 are not owned by the same user.")
            .add_option(["--old", "--old-containers"],
                box StoreConst(OldContainers),
                "Delete old containers. Currently it deletes all containers
                 not linked in .vagga/xxx directly. Basically it means keep
                 single container (of each name/variant) with version last used
                 for this specific contianer.")
            .add_option(["--transient"], box StoreConst(Transient),
                "Remove all transient containers, which do not have running
                 processes. Note running processes are matched by pid, so may
                 not be robust enough in some cases")
            .add_option(["--all-transient"],
                box StoreConst(AllTransient),
                "Remove all transient containers without liveness checking")
            .required();
        env_options(env, &mut ap);
        match ap.parse(args, &mut stdout(), &mut stderr()) {
            Ok(()) => {}
            Err(0) => return Ok(0),
            Err(_) => return Ok(122),
        }
    }

    let mut runenv = TreeMap::new();
    for &(ref k, ref v) in os::env().iter() {
        runenv.insert(k.clone(), v.clone());
    }
    env.populate_environ(&mut runenv);


    match mode {
        Help => return Err(format!("Use one of the cleanup options")),
        Container => {
            let roots = env.local_vagga.join(".roots");
            try!(run_rmdirs(&env.vagga_exe,
                names.iter().map(|n| roots.join(n.as_slice())).collect()));
        }
        Everything => {
            try!(run_rmdirs(&env.vagga_exe, vec!(env.local_vagga.clone())));
        }
        TmpFolders => {
            let roots = env.local_vagga.join(".roots");
            let arts = env.local_vagga.join(".artifacts");
            let mut to_delete = Vec::new();
            for d in [roots, arts].iter() {
                match readdir(d) {
                    Ok(items) => {
                        for path in items.iter() {
                            match path.extension_str() {
                                Some(x) if x.as_slice() == "tmp" => {
                                    to_delete.push(path.clone());
                                }
                                _ => continue,
                            }
                        }
                    }
                    Err(ref e) if e.kind == FileNotFound => {}
                    Err(ref e) => {
                        return Err(format!("Can't read dir {}: {}",
                            d.display(), e));
                    }
                }
            }
            try!(run_rmdirs(&env.vagga_exe, to_delete));
        }
        OldContainers => {
            let links = try!(readdir(&env.local_vagga)
                .map_err(|e| format!("Can't read dir {}: {}",
                    env.local_vagga.display(), e)));
            let mut roots = TreeSet::new();
            for item in links.iter() {
                if item.filename_str().unwrap().starts_with(".") { continue; }
                let lnk = try!(readlink(item)
                    .map_err(|e| format!("Can't read link {}: {}",
                        item.display(), e)));
                if lnk.dir_path() == Path::new(".roots") {
                    roots.insert(lnk.filename_str().unwrap().to_string());
                }
            }
            debug!("Roots alive: {}", roots);
            let items = try!(readdir(&env.local_vagga.join(".roots"))
                .map_err(|e| format!("Can't read dir {}/.roots: {}",
                    env.local_vagga.display(), e)));
            let mut to_delete = Vec::new();
            for path in items.iter() {
                match path.filename_str() {
                    Some(n) if roots.contains(&n.to_string()) => {
                        continue;
                    }
                    _ => { to_delete.push(path.clone()); }
                }
            }
            try!(run_rmdirs(&env.vagga_exe, to_delete));
        }
        AllTransient => {
            let transient = env.local_vagga.join(".transients");
            let mut to_delete = Vec::new();
            match readdir(&transient) {
                Ok(items) => {
                    for path in items.iter() {
                        to_delete.push(path.clone());
                    }
                }
                Err(ref e) if e.kind == FileNotFound => {}
                Err(ref e) => {
                    return Err(format!("Can't read dir {}: {}",
                        transient.display(), e));
                }
            }
            try!(run_rmdirs(&env.vagga_exe, to_delete));
        }
        Transient => {
            let transient = env.local_vagga.join(".transients");
            let mut to_delete = Vec::new();
            match readdir(&transient) {
                Ok(items) => {
                    for path in items.iter() {
                        let pid = match path.extension()
                            .and_then(from_utf8)
                            .and_then(from_str) {
                            None => continue,  // Not our dir, skip
                            Some(pid) => pid,
                        };
                        if Process::kill(pid, 0).ok().is_none() {
                            to_delete.push(path.clone());
                        }
                    }
                }
                Err(ref e) if e.kind == FileNotFound => {}
                Err(ref e) => {
                    return Err(format!("Can't read dir {}: {}",
                        transient.display(), e));
                }
            }
            try!(run_rmdirs(&env.vagga_exe, to_delete));
        }
    }

    return Ok(0);
}
