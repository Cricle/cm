DEFAULTS = {
    "old_timeout": 30,
    "retries": 3,
}


def load_settings(**overrides):
    return {**DEFAULTS, **overrides}
