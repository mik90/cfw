"""Duration-string parsing for Bazel schedule declarations."""

_DECIMAL_SCALES = [1, 10, 100, 1000, 10000, 100000, 1000000, 10000000, 100000000, 1000000000]

_DURATION_UNITS = {
    "ns": 1,
    "nanos": 1,
    "nanosecond": 1,
    "nanoseconds": 1,
    "us": 1000,
    "micros": 1000,
    "microsecond": 1000,
    "microseconds": 1000,
    "ms": 1000000,
    "millis": 1000000,
    "millisecond": 1000000,
    "milliseconds": 1000000,
    "s": 1000000000,
    "sec": 1000000000,
    "secs": 1000000000,
    "second": 1000000000,
    "seconds": 1000000000,
    "m": 60000000000,
    "min": 60000000000,
    "mins": 60000000000,
    "minute": 60000000000,
    "minutes": 60000000000,
    "h": 3600000000000,
    "hr": 3600000000000,
    "hrs": 3600000000000,
    "hour": 3600000000000,
    "hours": 3600000000000,
}

def duration_nanos(attr_name, value):
    """Parse a duration such as '100ms' or '1.5sec' into nanoseconds."""
    text = value.replace(" ", "").lower()
    if not text:
        fail("%s must be a duration such as '100ms'" % attr_name)

    split = len(text)
    for index in range(len(text)):
        char = text[index]
        if char not in "0123456789.":
            split = index
            break
    if split == 0 or split == len(text):
        fail("%s must be a number followed by a unit, got '%s'" % (attr_name, value))

    number = text[:split]
    unit = text[split:]
    if unit not in _DURATION_UNITS:
        fail(
            "%s has unknown unit '%s' in '%s'; expected one of: %s" % (
                attr_name,
                unit,
                value,
                ", ".join(sorted(_DURATION_UNITS.keys())),
            ),
        )

    parts = number.split(".")
    if len(parts) > 2:
        fail("%s has an invalid number in '%s'" % (attr_name, value))
    whole = parts[0]
    fraction = parts[1] if len(parts) == 2 else ""
    if (whole and not whole.isdigit()) or (fraction and not fraction.isdigit()):
        fail("%s has an invalid number in '%s'" % (attr_name, value))
    if len(fraction) > 9:
        fail("%s is more precise than one nanosecond in '%s'" % (attr_name, value))

    unit_nanos = _DURATION_UNITS[unit]
    whole_nanos = int(whole or "0") * unit_nanos
    fraction_nanos = 0
    if fraction:
        scale = _DECIMAL_SCALES[len(fraction)]
        scaled = int(fraction) * unit_nanos
        if scaled % scale:
            fail("%s is more precise than one nanosecond in '%s'" % (attr_name, value))
        fraction_nanos = scaled // scale
    return whole_nanos + fraction_nanos

def duration_expression(nanos):
    """Build a Rust expression for a nanosecond duration."""
    return "::core::time::Duration::from_nanos(%d)" % nanos
