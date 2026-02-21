"""Background job execution framework.

NOT for agent tool execution (which is always in-process).
This is for heavy async workloads: training, data collection, evaluation.
"""
