"""Dünner Wrapper um die Docker-CLI für SWE-bench-Per-Instance-Container.

Die offiziellen Eval-Images (swebench/sweb.eval.x86_64.<id>) enthalten das
Repo unter /testbed, ausgecheckt auf base_commit, samt fertig eingerichteter
Conda-Umgebung. Wir starten den Container mit `sleep infinity`, kopieren
Binary + Prompt hinein, lassen den Agent laufen und sammeln den Diff ein.
"""

from __future__ import annotations

import subprocess
from pathlib import Path


class ExecResult:
    def __init__(self, returncode: int, stdout: str, stderr: str):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def image_for(instance_id: str) -> str:
    # '__' im instance_id ist in Image-Namen als '_1776_' kodiert
    return f"swebench/sweb.eval.x86_64.{instance_id.replace('__', '_1776_')}:latest"


class SwebenchContainer:
    def __init__(self, image: str, platform: str | None = None, pull_timeout: int = 1800):
        self.image = image
        self.platform = platform
        self.pull_timeout = pull_timeout
        self.cid: str | None = None

    def __enter__(self) -> "SwebenchContainer":
        args = ["docker", "run", "-d", "--rm"]
        if self.platform:
            args += ["--platform", self.platform]
        args += [self.image, "sleep", "infinity"]
        proc = subprocess.run(args, capture_output=True, text=True, timeout=self.pull_timeout)
        if proc.returncode != 0:
            raise RuntimeError(f"docker run {self.image} fehlgeschlagen: {proc.stderr.strip()}")
        self.cid = proc.stdout.strip()
        return self

    def __exit__(self, *exc) -> None:
        if self.cid:
            subprocess.run(["docker", "rm", "-f", self.cid], capture_output=True, text=True)

    def copy_in(self, src: Path, dest: str) -> None:
        proc = subprocess.run(
            ["docker", "cp", str(src), f"{self.cid}:{dest}"],
            capture_output=True, text=True,
        )
        if proc.returncode != 0:
            raise RuntimeError(f"docker cp {src} fehlgeschlagen: {proc.stderr.strip()}")

    def exec(
        self,
        command: str,
        env: dict[str, str] | None = None,
        timeout: int | None = None,
        workdir: str = "/testbed",
    ) -> ExecResult:
        args = ["docker", "exec", "-w", workdir]
        for k, v in (env or {}).items():
            args += ["-e", f"{k}={v}"]
        args += [self.cid, "bash", "-lc", command]
        try:
            proc = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
            return ExecResult(proc.returncode, proc.stdout, proc.stderr)
        except subprocess.TimeoutExpired as e:
            out = e.stdout.decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
            err = e.stderr.decode() if isinstance(e.stderr, bytes) else (e.stderr or "")
            return ExecResult(124, out, err + f"\n[timeout nach {timeout}s]")


def remove_image(image: str) -> None:
    subprocess.run(["docker", "rmi", image], capture_output=True, text=True)
