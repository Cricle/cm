def mean(values):
    if not values:
        raise ValueError("mean requires at least one value")
    return sum(values) / len(values)
