"""Keep signing and native verification independent of storage and browsers."""

import subprocess


def check(package: str, tree: str) -> None:
    names = {line.split()[0] for line in tree.splitlines() if line.strip()}
    required = {package}
    forbidden = {"heddle-objects", "heddle-repo", "heddle-pack", "heddle-fs-prims", "sley", "sley-odb", "sley-transport"}
    if package == "heddleco-capability-verifier":
        required |= {"biscuit-auth", "ed25519-dalek"}
        forbidden |= {"wasm-bindgen", "js-sys", "web-sys"}
    elif package == "heddle-object-model":
        required |= {"heddle-format", "sley-core", "sley-object"}
    else:
        required |= {"heddle-object-model", "ed25519-dalek"}
    if not required <= names:
        raise ValueError(f"{package}: incomplete dependency inventory")
    if names & forbidden:
        raise ValueError(f"{package}: unexpected dependencies {sorted(names & forbidden)}")


if __name__ == "__main__":
    for package in ("heddle-object-model", "heddle-crypto", "heddleco-capability-verifier"):
        tree = subprocess.check_output(
            ["cargo", "tree", "--locked", "-p", package, "--edges", "normal",
             "--prefix", "none", "--format", "{p}"],
            text=True,
        )
        check(package, tree)
        print(f"{package}: dependency boundary holds")
