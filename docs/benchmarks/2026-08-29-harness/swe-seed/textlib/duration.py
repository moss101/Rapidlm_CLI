"""Compact human-readable durations: format and parse."""


def format_duration(seconds):
    """Render a number of seconds as a compact string like `1h 1m 1s`.

    Zero renders as `0s`. Negative values render with a leading `-`.
    Only the non-zero leading units are shown: 125 -> `2m 5s`.
    """
    sign = "-" if seconds < 0 else ""
    seconds = abs(int(seconds))
    hours = seconds // 3600
    minutes = seconds // 60
    secs = seconds % 60
    parts = []
    if hours:
        parts.append("%dh" % hours)
    if minutes:
        parts.append("%dm" % minutes)
    if secs or not parts:
        parts.append("%ds" % secs)
    return sign + " ".join(parts)


def parse_duration(text):
    """Parse a duration string like `1h 1m 1s` into seconds (int).

    Accepts a leading `-` for negative durations. Raises ValueError on
    malformed input.
    """
    body = text.strip()
    negative = body.startswith("-")
    body = body.lstrip("-").strip()
    if not body:
        raise ValueError("empty duration")
    total = 0
    for token in body.split():
        if token.endswith("h"):
            total += int(token[:-1]) * 3600
        elif token.endswith("m"):
            total += int(token[:-1]) * 60
        elif token.endswith("s"):
            total += int(token[:-1])
        else:
            raise ValueError("bad duration token: %r" % token)
    return -total if negative else total
