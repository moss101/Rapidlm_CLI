# textlib

Small text-processing utilities used by the ingestion pipeline.

Modules:

- `textlib.lru` — `LRUCache`, a size-bounded mapping that evicts its
  least-recently-used entry.
- `textlib.csvlite` — `parse_csv_line`, RFC-4180-style single-line CSV
  parsing with quotes and doubled-quote escapes.
- `textlib.duration` — `format_duration` / `parse_duration` for compact
  human durations like `1h 1m 1s`.
- `textlib.prio` — `PriorityQueue`: lower priority number pops first,
  equal priorities pop in insertion order.
- `textlib.tokenize` — `words`: split text into word tokens
  (hyphenated words and internal apostrophes stay intact).

Run the test suite:

    python3 -m unittest discover -s tests -v
