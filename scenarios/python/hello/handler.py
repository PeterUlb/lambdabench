# Scenario "hello": return a constant.
# Baseline isolating Python runtime startup + handler dispatch, no I/O, no SDK.
# The returned constant matches the other languages' hello output, so only
# runtime overhead differs.


def handler(event, context):
    return {"message": "hello", "scenario": "hello"}
