# Paper Shader Compiler

This tool compiles all the shaders used by Escher's paper renderer. The resulting compiled shader spirv
binary is then saved to disk in src/ui/lib/escher/shaders/spirv. The name for the file is auto_generated
based on the input name of the original shader plus a hash value calculated from the list of shader
variant arguments.

To use:

1) Migrate to your fuchsia root directory.

2) fx set workbench_eng.x64 --with //src/ui/tools:scenic

3) fx build --host //src/ui/tools/paper_shader_compiler

4) fx host-tool paper_shader_compiler
