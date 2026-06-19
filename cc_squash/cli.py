from __future__ import annotations

import click
from loguru import logger


@click.group()
@click.version_option(package_name="cc-squash")
def main() -> None:
    """Augmented auto-compaction for long-running Claude Code sessions."""


@main.command()
def hello() -> None:
    """Print a greeting — the starter command."""
    logger.debug("hello invoked")
    click.echo("Hello from cc-squash!")
