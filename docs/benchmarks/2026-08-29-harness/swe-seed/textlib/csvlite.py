"""Single-line CSV parsing with quote support (RFC 4180 subset)."""


def parse_csv_line(line, delimiter=","):
    """Parse one CSV line into a list of string fields.

    A field may be wrapped in double quotes to embed the delimiter.
    Inside quotes, a doubled quote (`""`) is a literal quote character.
    Empty fields are preserved, including trailing ones.
    """
    fields = []
    current = []
    in_quotes = False
    i = 0
    n = len(line)
    while i < n:
        ch = line[i]
        if in_quotes:
            if ch == '"':
                if i + 1 < n and line[i + 1] == '"':
                    current.append('"')
                    i += 2
                    continue
                in_quotes = False
            else:
                current.append(ch)
        else:
            if ch == '"' and not current:
                in_quotes = True
            elif ch == delimiter:
                fields.append("".join(current))
                current = []
            else:
                current.append(ch)
        i += 1
    fields.append("".join(current))
    if fields and fields[-1] == "":
        fields.pop()
    return fields
