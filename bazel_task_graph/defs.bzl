"""Public entry point for cfw Bazel task composition."""

load(
    "//bazel_task_graph:channel.bzl",
    _CfwChannelInfo = "CfwChannelInfo",
    _cfw_channel = "cfw_channel",
)
load(
    "//bazel_task_graph:live_executor.bzl",
    _cfw_live_application = "cfw_live_application",
)
load(
    "//bazel_task_graph:pools.bzl",
    _CfwPoolInfo = "CfwPoolInfo",
    _cfw_pool = "cfw_pool",
)
load(
    "//bazel_task_graph:publisher.bzl",
    _CfwPublisherInfo = "CfwPublisherInfo",
    _cfw_publisher = "cfw_publisher",
)
load(
    "//bazel_task_graph:schedule.bzl",
    _CfwCallbackScheduleInfo = "CfwCallbackScheduleInfo",
    _cfw_custom_schedule = "cfw_custom_schedule",
    _cfw_fixed_schedule = "cfw_fixed_schedule",
)
load(
    "//bazel_task_graph:subscriber.bzl",
    _CfwSubscriberInfo = "CfwSubscriberInfo",
    _cfw_subscriber = "cfw_subscriber",
)
load(
    "//bazel_task_graph:task.bzl",
    _CfwTaskInfo = "CfwTaskInfo",
    _cfw_task = "cfw_task",
)

CfwCallbackScheduleInfo = _CfwCallbackScheduleInfo
CfwChannelInfo = _CfwChannelInfo
CfwPoolInfo = _CfwPoolInfo
CfwPublisherInfo = _CfwPublisherInfo
CfwSubscriberInfo = _CfwSubscriberInfo
CfwTaskInfo = _CfwTaskInfo
cfw_channel = _cfw_channel
cfw_custom_schedule = _cfw_custom_schedule
cfw_fixed_schedule = _cfw_fixed_schedule
cfw_live_application = _cfw_live_application
cfw_pool = _cfw_pool
cfw_publisher = _cfw_publisher
cfw_subscriber = _cfw_subscriber
cfw_task = _cfw_task
