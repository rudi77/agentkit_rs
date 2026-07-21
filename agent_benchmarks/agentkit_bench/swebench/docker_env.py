"""Dünner Wrapper um die Docker-CLI für SWE-bench-Per-Instance-Container.

Die offiziellen Eval-Images (swebench/sweb.eval.x86_64.<id>) enthalten das
Repo unter /testbed, ausgecheckt auf base_commit, samt fertig eingerichteter
Conda-Umgebung. Wir starten den Container mit `sleep infinity`, kopieren
Binary + Prompt hinein, lassen den Agent laufen und sammeln den Diff ein.
"""

from __future__ import annotations

import subprocess
import threading
import time
from pathlib import Path


class ExecResult:
    def __init__(self, returncode: int, stdout: str, stderr: str):
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = stderr


def image_for(instance_id: str) -> str:
    # '__' im instance_id ist in Image-Namen als '_1776_' kodiert
    return f"swebench/sweb.eval.x86_64.{instance_id.replace('__', '_1776_')}:latest"


# Mehr als 2 parallele Pulls der GB-großen Eval-Images lassen die
# CloudFront-Downloads von Docker Hub regelmäßig mit EOF abbrechen.
_PULL_GATE = threading.Semaphore(2)


def _image_present(image: str) -> bool:
    return subprocess.run(
        ["docker", "image", "inspect", image], capture_output=True
    ).returncode == 0


def ensure_image(
    image: str, platform: str | None = None, timeout: int = 1800, retries: int = 6
) -> None:
    """Image explizit pullen — gedrosselt und mit Backoff-Retry.

    `docker run` würde implizit pullen, aber ohne Retry: ein transienter
    Netzwerkfehler (EOF, Rate-Limit) lässt dann die ganze Instanz scheitern.
    """
    if _image_present(image):
        return
    with _PULL_GATE:
        if _image_present(image):  # anderer Worker war schneller
            return
        args = ["docker", "pull"]
        if platform:
            args += ["--platform", platform]
        args.append(image)
        last_err = ""
        for attempt in range(1, retries + 1):
            proc = subprocess.run(
                args, capture_output=True, text=True,
                encoding="utf-8", errors="replace", timeout=timeout,
            )
            if proc.returncode == 0:
                return
            last_err = proc.stderr.strip()
            if attempt < retries:
                # EOF-Abbrüche des CDN erholen sich meist erst nach Minuten;
                # Docker setzt auf bereits geladenen Layern wieder auf.
                time.sleep(min(15 * 2 ** (attempt - 1), 240))
        # Manche Blobs scheitern über CloudFront dauerhaft (EOF bei jedem
        # Versuch) — Googles Docker-Hub-Mirror umgeht das CDN.
        if _pull_via_mirror(image, platform, timeout):
            return
        raise RuntimeError(
            f"docker pull {image} nach {retries} Versuchen fehlgeschlagen: {last_err}"
        )


def _pull_via_mirror(image: str, platform: str | None, timeout: int) -> bool:
    """Fallback: Image über mirror.gcr.io ziehen und auf den Docker-Hub-Namen taggen."""
    mirror = f"mirror.gcr.io/{image}"
    args = ["docker", "pull"]
    if platform:
        args += ["--platform", platform]
    proc = subprocess.run(
        args + [mirror], capture_output=True, text=True,
        encoding="utf-8", errors="replace", timeout=timeout,
    )
    if proc.returncode != 0:
        return False
    tag = subprocess.run(
        ["docker", "tag", mirror, image], capture_output=True, text=True,
        encoding="utf-8", errors="replace",
    )
    subprocess.run(["docker", "rmi", mirror], capture_output=True)
    return tag.returncode == 0


class SwebenchContainer:
    def __init__(self, image: str, platform: str | None = None, pull_timeout: int = 1800):
        self.image = image
        self.platform = platform
        self.pull_timeout = pull_timeout
        self.cid: str | None = None

    def __enter__(self) -> "SwebenchContainer":
        ensure_image(self.image, self.platform, self.pull_timeout)
        args = ["docker", "run", "-d", "--rm"]
        if self.platform:
            args += ["--platform", self.platform]
        args += [self.image, "sleep", "infinity"]
        proc = subprocess.run(
            args, capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=self.pull_timeout,
        )
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
            capture_output=True, text=True, encoding="utf-8", errors="replace",
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
        # encoding explizit: text=True nähme sonst die Windows-Codepage (cp1252)
        # und stolpert über Unicode im Agent-Output/Diff.
        try:
            proc = subprocess.run(
                args, capture_output=True, text=True,
                encoding="utf-8", errors="replace", timeout=timeout,
            )
            return ExecResult(proc.returncode, proc.stdout, proc.stderr)
        except subprocess.TimeoutExpired as e:
            def _dec(x):
                if isinstance(x, bytes):
                    return x.decode("utf-8", errors="replace")
                return x or ""
            return ExecResult(124, _dec(e.stdout), _dec(e.stderr) + f"\n[timeout nach {timeout}s]")


def remove_image(image: str) -> None:
    subprocess.run(["docker", "rmi", image], capture_output=True, text=True)
