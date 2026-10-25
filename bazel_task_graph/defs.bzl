"""Public entry point for cfw Bazel task composition."""

load("//bazel_task_graph:channel.bzl", _CfwChannelInfo = "CfwChannelInfo", _channel = "channel")
load(
    "//bazel_task_graph:live_executor.bzl",
    _live_application = "live_application",
)
load("//bazel_task_graph:pools.bzl", _CfwPoolInfo = "CfwPoolInfo", _pool = "pool")
load(
    "//bazel_task_graph:publisher.bzl",
    _CfwPublisherInfo = "CfwPublisherInfo",
    _publisher = "publisher",
)
load(
    "//bazel_task_graph:schedule.bzl",
    _CfwCallbackScheduleInfo = "CfwCallbackScheduleInfo",
    _custom_schedule = "custom_schedule",
    _fixed_schedule = "fixed_schedule",
)
load(
    "//bazel_task_graph:subscriber.bzl",
    _CfwSubscriberInfo = "CfwSubscriberInfo",
    _subscriber = "subscriber",
)
load("//bazel_task_graph:task.bzl", _CfwTaskInfo = "CfwTaskInfo", _task = "task")

CfwCallbackScheduleInfo = _CfwCallbackScheduleInfo
CfwChannelInfo = _CfwChannelInfo
CfwPoolInfo = _CfwPoolInfo
CfwPublisherInfo = _CfwPublisherInfo
CfwSubscriberInfo = _CfwSubscriberInfo
CfwTaskInfo = _CfwTaskInfo
channel = _channel
custom_schedule = _custom_schedule
fixed_schedule = _fixed_schedule
live_application = _live_application
pool = _pool
publisher = _publisher
subscriber = _subscriber
task = _task
