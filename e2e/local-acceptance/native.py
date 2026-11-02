"""Run host-specific acceptance through real native Cargo, Git and SSH processes."""

import os
from pathlib import Path
import sys
import tempfile

from run import SOURCE, command
from resume_acceptance import verify_resume


def windows_color_acceptance(directory):
    if os.name != "nt":
        return
    # Launch real Cargo through the production executor, including VsDevCmd.
    # A preinitialized Developer shell must not hide the reinjection regression.
    crate = directory / "platform-color"
    (crate / "src").mkdir(parents=True)
    (crate / "Cargo.toml").write_text('[package]\nname="platform-color-probe"\nversion="0.0.1"\nedition="2021"\n\n[workspace]\n')
    (crate / "Cargo.lock").write_text('version = 4\n\n[[package]]\nname = "platform-color-probe"\nversion = "0.0.1"\n')
    (crate / "src/main.rs").write_text('fn main() { println!("{:?}", std::env::var_os("NO_COLOR")); }\n')
    code = ("import importlib.util,os,sys; from pathlib import Path; "
            "os.environ.pop('LIB',None); "
            "spec=importlib.util.spec_from_file_location('acceptance',sys.argv[1]); "
            "module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module); "
            "root=Path(sys.argv[2]); "
            "args=['cargo','run','--offline','--locked','--quiet','--manifest-path',str(root/'Cargo.toml'),'--target-dir',str(root/'target')]; "
            "default=root/'default.log'; explicit=root/'explicit.log'; "
            "module.run_command(args,root,default); "
            "assert module.MSVC_ENV is not None and module.MSVC_ENV.get('NO_COLOR')=='1', 'Actual VsDevCmd environment was not exercised'; "
            "assert default.read_text().splitlines()[-1]=='None', 'Platform environment suppressed default colors'; "
            "module.run_command(args,root,explicit,{'NO_COLOR':'1'}); "
            "assert explicit.read_text().splitlines()[-1]=='Some(\"1\")', 'Explicit color opt-out was lost'")
    command(sys.executable, "-c", code, SOURCE / ".github/scripts/local_acceptance.py", crate, cwd=SOURCE)
    print("PASS: actual Windows Cargo/Visual Studio initialization cleans inherited NO_COLOR and preserves explicit opt-out")


def main():
    sys.stdout.reconfigure(encoding="utf-8", line_buffering=True)
    sys.stderr.reconfigure(encoding="utf-8", line_buffering=True)
    with tempfile.TemporaryDirectory(prefix="scode-native-guard-") as temporary:
        directory = Path(temporary)
        windows_color_acceptance(directory)
        verify_resume(directory, SOURCE, command, host_smoke=True)
    print("PASS: native host recovery and platform initialization guards")


if __name__ == "__main__":
    main()
