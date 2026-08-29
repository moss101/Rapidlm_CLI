"""LRUCache: a size-bounded mapping with least-recently-used eviction."""


class LRUCache:
    """Fixed-capacity cache that evicts its least-recently-used entry.

    Recency is refreshed by both `put` and `get`. Eviction keeps the
    `capacity` most recently used keys.
    """

    def __init__(self, capacity):
        if capacity <= 0:
            raise ValueError("capacity must be positive")
        self.capacity = capacity
        self._data = {}
        self._order = []  # least-recently-used first

    def __len__(self):
        return len(self._data)

    def __contains__(self, key):
        return key in self._data

    def keys(self):
        """Snapshot of the cached keys, least-recently-used first."""
        return list(self._order)

    def _touch(self, key):
        """Mark `key` as the most recently used entry."""
        self._order.remove(key)
        self._order.append(key)

    def put(self, key, value):
        if key in self._data:
            self._touch(key)
        else:
            self._order.append(key)
        self._data[key] = value
        while len(self._data) > self.capacity:
            oldest = self._order.pop(0)
            del self._data[oldest]

    def get(self, key, default=None):
        if key not in self._data:
            return default
        return self._data[key]
