class Cache:
    def __init__(self, capacity):
        self.capacity = capacity
        self.items = {}

    def get(self, key):
        return self.items.get(key)

    def put(self, key, value):
        if len(self.items) >= self.capacity:
            oldest = next(iter(self.items))
            del self.items[oldest]
        self.items[key] = value
