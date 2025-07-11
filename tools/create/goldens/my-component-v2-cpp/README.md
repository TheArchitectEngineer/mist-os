# my-component-v2-cpp

TODO: Brief overview of the component.

## Building

To add this component to your build, append
`--with //tools/create/goldens/my-component-v2-cpp`
to the `fx set` invocation.

## Running

Use `ffx component run` to launch this component into a restricted realm
for development purposes:

```
$ ffx component run /core/ffx-laboratory:my-component-v2-cpp fuchsia-pkg://fuchsia.com/my-component-v2-cpp#meta/my-component-v2-cpp.cm
```

## Testing

Unit tests for my-component-v2-cpp are available in the `my-component-v2-cpp-tests`
package.

```
$ fx test my-component-v2-cpp-tests
```

