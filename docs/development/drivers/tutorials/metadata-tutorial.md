# Passing metadata between drivers tutorial

This guide explains how to pass metadata from one driver to another using the
[metadata](/sdk/lib/driver/metadata) library. Metadata is any arbitrary data
created by a driver that should be accessible to drivers bound to its child
nodes. Normally, drivers can create their own FIDL protocol in order to pass
metadata, however, the [metadata](/sdk/lib/driver/metadata) library offers
functionality to pass metadata between drivers with less code.

You can find an example of drivers sending, retrieving and forwarding metadata
[here](/examples/drivers/metadata).

## Metadata definition
First, the type of metadata must be defined as a FIDL type and annotated with
`@serializable`:

```
library fuchsia.examples.metadata;

// Type of the metadata to be passed.
// Make sure to annotate it with `@serializable`.
@serializable
type Metadata = table {
    1: test_property string:MAX;
};
```

This metadata will be served in the driver's outgoing namespace using the
[fuchsia.driver.metadata/Service](/sdk/fidl/fuchsia.driver.metadata/fuchsia.driver.metadata.fidl)
FIDL service. The name of the FIDL service within the driver's outgoing
namespace will not be `fuchsia.driver.metadata.Service`. Instead, it will be the
serializable name of the FIDL type. The serializable name is only created if the
FIDL type is annotated with `@serializable`.

The FIDL library's build target will be defined as the following:

```
fidl("fuchsia.examples.metadata") {
  sources = [ "fuchsia.examples.metadata.fidl" ]
}
```

## Sending metadata
### Initial setup
Let's say we have a driver that wants to send metadata to its children:

```cpp
#include <lib/driver/component/cpp/driver_export.h>

class Sender : public fdf::DriverBase {
 public:
  Sender(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase("sender", std::move(start_args), std::move(driver_dispatcher)) {}
};

FUCHSIA_DRIVER_EXPORT(Sender);
```

It's component manifest is the following:

```
{
    include: [
        "inspect/client.shard.cml",
        "syslog/client.shard.cml",
    ],
    program: {
        runner: "driver",
        binary: "driver/sender.so",
        bind: "meta/bind/sender.bindbc",
    },
}
```

It's build targets are defined as follows:

```
fuchsia_cc_driver("driver") {
  testonly = true
  output_name = "sender"
  sources = [
    "sender.cc",
  ]
  deps = [
    "//src/devices/lib/driver:driver_runtime",
  ]
}

fuchsia_driver_component("component") {
  testonly = true
  component_name = "sender"
  manifest = "meta/sender.cml"
  deps = [
    ":driver",
    ":bind", # Bind rules not specified in this tutorial.
  ]
  info = "sender.json" # Info not specified in this tutorial.
}
```

### Send process
In order for this driver to send metadata to its child drivers, it will need to
create an instance of the
[fdf_metadata::MetadataServer](/sdk/lib/driver/metadata/cpp/metadata_server.h)
class, set the metadata by calling its
`fdf_metadata::MetadataServer::SetMetadata()` method, serve the
metadata to the driver's outgoing directory by calling its
`fdf_metadata::MetadataServer::Serve()` method, and pass the metadata
server's offer created by `fdf_metadata::MetadataServer::MakeOffer()` to the
child node:

```cpp
{% verbatim %}
// Defines the fuchsia.examples.metadata types in C++.
#include <fidl/fuchsia.examples.metadata/cpp/fidl.h>

// Defines fdf::MetadataServer.
#include <lib/driver/metadata/cpp/metadata_server.h>

class Sender : public fdf::DriverBase {
 public:
  zx::result<> Start() override {
    // Set the metadata to be served.
    fuchsia_examples_metadata::Metadata metadata{{.test_property = "test value"}};
    ZX_ASSERT(metadata_server_.SetMetadata(std::move(metadata)).is_ok());

    // Serve the metadata to the driver's outgoing directory.
    ZX_ASSERT(metadata_server_.Serve(*outgoing(), dispatcher()).is_ok());

    // The metadata server's offer must be provided to the child node.
    std::vector<fuchsia_driver_framework::Offer> offers{
      metadata_server_.MakeOffer()};

    zx::result child = AddChild("child", {}, offers);
    if (child.is_error()) {
      return child.take_error();
    }

    return zx::ok();
  }

 private:
  // Responsible for serving metadata.
  fdf_metadata::MetadataServer<fuchsia_examples_metadata::Metadata> metadata_server_;
};
{% endverbatim %}
```

`fdf_metadata::MetadataServer::SetMetadata()` can be called multiple times,
before or after `fdf_metadata::MetadataServer::Serve()`, and the metadata server
will serve the latest metadata instance. A child driver will fail to retrieve
the metadata if `fdf_metadata::MetadataServer::SetMetadata()` has not been
called before it attempts to retrieve the metadata.

The `driver` build target will need to be updated to include the metadata
library and the FIDL library that defines the metadata type:

```
fuchsia_cc_driver("driver") {
  testonly = true
  output_name = "sender"
  sources = [
    "sender.cc",
  ]
  deps = [
    "//src/devices/lib/driver:driver_runtime",

    # This should be the fuchsia.examples.metadata FIDL library's C++ target.
    ":fuchsia.examples.metadata_cpp",

    # This library contains `fdf::MetadataServer`.
    "//sdk/lib/driver/metadata/cpp",
  ]
}
```

### Exposing the metadata service
Finally, the driver needs to declare and expose the
`fuchsia.examples.metadata.Metadata` FIDL service in its component manifest:

```
{
    include: [
        "inspect/client.shard.cml",
        "syslog/client.shard.cml",
    ],
    program: {
        runner: "driver",
        binary: "driver/sender.so",
        bind: "meta/bind/sender.bindbc",
    },
    capabilities: [
        { service: "fuchsia.examples.metadata.Metadata" },
    ],
    expose: [
        {
            service: "fuchsia.examples.metadata.Metadata",
            from: "self",
        },
    ],
}
```

Technically, there is no `fuchsia.examples.metadata.Metadata` FIDL service.
Under the hood,
[fdf_metadata::MetadataServer](/sdk/lib/driver/metadata/cpp/metadata_server.h)
serves the
[fuchsia.driver.metadata/Service](/sdk/fidl/fuchsia.driver.metadata/fuchsia.driver.metadata.fidl)
FIDL service in order to send metadata. However,
[fdf_metadata::MetadataServer](/sdk/lib/driver/metadata/cpp/metadata_server.h)
does not serve this service under the name `fuchsia.driver.metadata.Service`
like a normal FIDL service would. Instead, the service is served under the
metadata type's serializable name. In this case that is
`fuchsia.examples.metadata.Metadata`. This name can be found in the C++ field
`fuchsia_examples_metadata::Metadata::kSerializableName`. This allows the
receiving driver to identify which type of metadata is being passed. This means
that the driver needs to declare and expose the
`fuchsia.examples.metadata.Metadata` FIDL service even though that service
doesn't exist.

## Retrieving metadata
### Initial setup
Let's say we have a driver that wants to retrieve metadata from its parent
driver:

```cpp
#include <lib/driver/component/cpp/driver_export.h>

class Retriever : public fdf::DriverBase {
 public:
  Retriever(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase("retriever", std::move(start_args), std::move(driver_dispatcher)) {}
};

FUCHSIA_DRIVER_EXPORT(Retriever);
```

It's component manifest is the following:

```
{
    include: [
        "inspect/client.shard.cml",
        "syslog/client.shard.cml",
    ],
    program: {
        runner: "driver",
        binary: "driver/retriever.so",
        bind: "meta/bind/retriever.bindbc",
    },
}
```

It's build targets are defined as follows:

```
fuchsia_cc_driver("driver") {
  testonly = true
  output_name = "retriever"
  sources = [
    "retriever.cc",
  ]
  deps = [
    "//src/devices/lib/driver:driver_runtime",
  ]
}

fuchsia_driver_component("component") {
  testonly = true
  component_name = "retriever"
  manifest = "meta/retriever.cml"
  deps = [
    ":driver",
    ":bind", # Bind rules not specified in this tutorial.
  ]
  info = "retriever.json" # Info not specified in this tutorial.
}
```

### Retrieval process
In order for the driver to retrieve metadata from its parent driver, the driver
needs to call `fdf_metadata::GetMetadata()`:

```cpp
// Defines the fuchsia.examples.metadata types in C++.
#include <fidl/fuchsia.examples.metadata/cpp/fidl.h>

// Defines fdf::GetMetadata().
#include <lib/driver/metadata/cpp/metadata.h>

class Retriever : public fdf::DriverBase {
 public:
  zx::result<> Start() override {
    zx::result<fuchsia_examples_metadata::Metadata> metadata =
      fdf_metadata::GetMetadata<fuchsia_examples_metadata::Metadata>(*incoming());
    ZX_ASSERT(!metadata.is_error());

    return zx::ok();
  }
};
```

The `driver` build target will need to be updated to depend on the metadata
library and the FIDL library that defines the metadata type:

```
fuchsia_cc_driver("driver") {
  testonly = true
  output_name = "retriever"
  sources = [
    "retriever.cc",
  ]
  deps = [
    "//src/devices/lib/driver:driver_runtime",

    # This should be the fuchsia.examples.metadata FIDL library's C++ target.
    ":fuchsia.examples.metadata_cpp",

    # This library contains `fdf::GetMetadata()`.
    "//sdk/lib/driver/metadata/cpp",
  ]
}
```

### Using the metadata service
Finally, the driver will need to declare its usage of the
`fuchsia.examples.metadata.Metadata` FIDL service in its component manifest:

```
{
    include: [
        "inspect/client.shard.cml",
        "syslog/client.shard.cml",
    ],
    program: {
        runner: "driver",
        binary: "driver/retriever.so",
        bind: "meta/bind/retriever.bindbc",
    },
    use: [
        { service: "fuchsia.examples.metadata.Metadata" },
    ],
}
```

## Forwarding metadata
### Initial setup
Let's say we have a driver that wants to retrieve metadata from its parent
driver and forward that metadata to its child drivers:

```cpp
#include <lib/driver/component/cpp/driver_export.h>

class Forwarder : public fdf::DriverBase {
 public:
  Forwarder(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase("forwarder", std::move(start_args), std::move(driver_dispatcher)) {}
};

FUCHSIA_DRIVER_EXPORT(Forwarder);
```

It's component manifest is the following:

```
{
    include: [
        "inspect/client.shard.cml",
        "syslog/client.shard.cml",
    ],
    program: {
        runner: "driver",
        binary: "driver/forwarder.so",
        bind: "meta/bind/forwarder.bindbc",
    },
}
```

It's build targets are defined as follows:

```
fuchsia_cc_driver("driver") {
  testonly = true
  output_name = "forwarder"
  sources = [
    "forwarder.cc",
  ]
  deps = [
    "//src/devices/lib/driver:driver_runtime",
  ]
}

fuchsia_driver_component("component") {
  testonly = true
  component_name = "forward"
  manifest = "meta/forward.cml"
  deps = [
    ":driver",
    ":bind", # Bind rules not specified in this tutorial.
  ]
  info = "forward.json" # Info not specified in this tutorial.
}
```

### Forward process
In order for the driver to forward metadata from its parent driver to its child
drivers, it will need to create an instance of the
[fdf_metadata::MetadataServer](/sdk/lib/driver/metadata/cpp/metadata_server.h)
class, set the metadata by calling its
`fdf_metadata::MetadataServer::ForwardMetadata()` method, serve the
metadata to the driver's outgoing directory by calling its
`fdf_metadata::MetadataServer::Serve()` method, and pass the metadata server's
offer created by `fdf_metadata::MetadataServer::MakeOffer()` to the child node:

```cpp
// Defines the fuchsia.examples.metadata types in C++.
#include <fidl/fuchsia.examples.metadata/cpp/fidl.h>

// Defines fdf::MetadataServer.
#include <lib/driver/metadata/cpp/metadata_server.h>

class Forwarder : public fdf::DriverBase {
 public:
  zx::result<> Start() override {
    // Set metadata using the driver's parent driver metadata.
    ZX_ASSERT(metadata_server_.ForwardMetadata(incoming()),is_ok());

    // Serve the metadata to the driver's outgoing directory.
    ZX_ASSERT(metadata_server_.Serve(*outgoing(), dispatcher()).is_ok());

    // The metadata server's offer must be provided to the child node.
    std::vector<fuchsia_driver_framework::Offer> offers{
      metadata_server_.MakeOffer()};

    zx::result child = AddChild("child", {}, offers);
    if (child.is_error()) {
      return child.take_error();
    }

    return zx::ok();
  }

 private:
  // Responsible for serving metadata.
  fdf_metadata::MetadataServer<fuchsia_examples_metadata::Metadata> metadata_server_;
};
```

It should be noted that `fdf_metadata::MetadataServer::ForwardMetadata()` does
not check if the parent driver has changed what metadata it provides after
`fdf_metadata::MetadataServer::ForwardMetadata()` has been called. The driver
will have to call `fdf_metadata::MetadataServer::ForwardMetadata()` again in
order to incorporate the change.

The `driver` build target will need to be updated to depend on the metadata
library and FIDL library that defines the metadata type:

```
fuchsia_cc_driver("driver") {
  testonly = true
  output_name = "forwarder"
  sources = [
    "forwarder.cc",
  ]
  deps = [
    "//src/devices/lib/driver:driver_runtime",

    # This should be the fuchsia.examples.metadata FIDL library's C++ target.
    ":fuchsia.examples.metadata_cpp",

    # This library contains `fdf::MetadataServer`.
    "//sdk/lib/driver/metadata/cpp",
  ]
}
```

### Exposing and using the metadata service
Finally, the driver will need to declare, use, and expose the
`fuchsia.examples.metadata.Metadata` FIDL service in its component manifest:

```
{
    include: [
        "inspect/client.shard.cml",
        "syslog/client.shard.cml",
    ],
    program: {
        runner: "driver",
        binary: "driver/forwarder.so",
        bind: "meta/bind/forwarder.bindbc",
    },
    capabilities: [
        { service: "fuchsia.examples.metadata.Metadata" },
    ],
    expose: [
        {
            service: "fuchsia.examples.metadata.Metadata",
            from: "self",
        },
    ],
    use: [
        { service: "fuchsia.examples.metadata.Metadata" },
    ],
}
```
