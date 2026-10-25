"""Utilities for cleaning up labels."""

def crate_name(label):
    """Cleans up a rust crate name so that it follows our bazel '_" pattern"""
    return label.name.replace("-", "_")

def field_name(name):
    """Cleans up a rust field name so that it follows our bazel '_" pattern"""
    return name.replace("-", "_")

def info_label(label):
    """Given a label, adds an '_info' suffix."""
    label = str(label)
    package, target = label.rsplit(":", 1)
    return package + ":" + target + "_info"
