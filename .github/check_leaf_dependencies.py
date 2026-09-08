"""Keep signing and native verification independent of storage and browsers."""

import subprocess


def check(package: str, tree: str) -> None:
    names = {line.split()[0] for line in tree.splitlines() if line.strip()}
    required = {package, "ed25519-dalek"}
    forbidden = {"heddle-objects", "heddle-repo", "heddle-pack", "heddle-fs-prims"}
    if package == "heddleco-capability-verifier":
        required.add("biscuit-auth")
        forbidden |= {"wasm-bindgen", "js-sys", "web-sys"}
    else:
        required.add("heddle-object-model")
    if not required <= names:
        raise ValueError(f"{package}: incomplete dependency inventory")
    if names & forbidden:
        raise ValueError(f"{package}: unexpected dependencies {sorted(names & forbidden)}")


if __name__ == "__main__":
    for package in ("heddle-crypto", "heddleco-capability-verifier"):
        tree = subprocess.check_output(
            ["cargo", "tree", "--locked", "-p", package, "--edges", "normal",
             "--prefix", "none", "--format", "{p}"],
            text=True,
        )
        check(package, tree)
        print(f"{package}: dependency boundary holds")
