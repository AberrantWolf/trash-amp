"""Shared version policy for macOS alpha and beta packages."""
import re
import sys


def bundle_versions(version):
    match = re.fullmatch(
        r"((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))"
        r"-(alpha|beta)\.([1-9][0-9]?)", version
    )
    if not match:
        raise ValueError(
            f"Unsupported prerelease version {version!r}; expected X.Y.Z-alpha.N "
            "or X.Y.Z-beta.N, with N from 1 to 99."
        )
    short_version, channel, sequence = match.groups()
    # The channel sorts before the sequence: beta.1 follows every alpha build.
    channel_number = {"alpha": 1, "beta": 2}[channel]
    return short_version, f"{channel_number}.{sequence}"


if __name__ == "__main__":
    try:
        print(*bundle_versions(sys.argv[1]))
    except (IndexError, ValueError) as error:
        raise SystemExit(str(error))
