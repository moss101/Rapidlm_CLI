"""Word tokenization for pipeline indexing."""

import re

# A word is a run of alphanumerics; hyphens and apostrophes join runs
# when they sit strictly inside a word ("state-of-the-art", "don't").
_TOKEN = re.compile(r"[A-Za-z0-9']+")


def words(text):
    """Return the word tokens of `text`, in order.

    Tokens are runs of alphanumerics. A hyphen or apostrophe is kept as
    part of a token only when it sits between two alphanumerics, so
    hyphenated compounds stay whole and possessives keep their mark.
    """
    return _TOKEN.findall(text)
