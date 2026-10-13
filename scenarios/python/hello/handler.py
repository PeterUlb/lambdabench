# Scenario "hello": return a constant.
# Baseline isolating Python runtime startup + handler dispatch, no I/O, no SDK.
# The returned constant matches the other languages' hello output, so only
# runtime overhead differs.


def handler(event, context):
    # The bencher's unrecorded host-CPU invoke (see lambdabench_cpuinfo.py).
    if context.client_context and (context.client_context.custom or {}).get("lambdabench") == "cpu":
        import lambdabench_cpuinfo

        lambdabench_cpuinfo.report()
    return {"message": "hello", "scenario": "hello"}
