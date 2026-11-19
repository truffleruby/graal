#
# Copyright (c) 2025, 2026, Oracle and/or its affiliates. All rights reserved.
# DO NOT ALTER OR REMOVE COPYRIGHT NOTICES OR THIS FILE HEADER.
#
# The Universal Permissive License (UPL), Version 1.0
#
# Subject to the condition set forth below, permission is hereby granted to any
# person obtaining a copy of this software, associated documentation and/or
# data (collectively the "Software"), free of charge and under any and all
# copyright rights in the Software, and any and all patent rights owned or
# freely licensable by each licensor hereunder covering either (i) the
# unmodified Software as contributed to or provided by such licensor, or (ii)
# the Larger Works (as defined below), to deal in both
#
# (a) the Software, and
#
# (b) any piece of software and/or hardware listed in the lrgrwrks.txt file if
# one is included with the Software each a "Larger Work" to which the Software
# is contributed by such licensors),
#
# without restriction, including without limitation the rights to copy, create
# derivative works of, display, perform, and distribute the Software and make,
# use, sell, offer for sale, import, export, have made, and have sold the
# Software and the Larger Work(s), and to sublicense the foregoing rights on
# either these or other terms.
#
# This license is subject to the following condition:
#
# The above copyright notice and either this complete permission notice or at a
# minimum a reference to the UPL must be included in all copies or substantial
# portions of the Software.
#
# THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
# IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
# FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
# AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
# LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
# OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
# SOFTWARE.
#

"""
Builds the thin launchers of languages (`launcher.rs` in `org.graalvm.launcher.native`) with `rustc`.
"""

import json
import os
import shutil
from os.path import join, dirname, basename, exists

import mx
import mx_subst
import mx_util


class RustLauncherProject(mx.AbstractNativeProject):
    """
    Base class of the projects building the thin launcher of a language (`launcher.rs` in
    `org.graalvm.launcher.native`), a single Rust source file without dependencies compiled directly
    with `rustc`. Since the Rust standard library is linked statically, the launcher only depends on
    the C library and no C++ toolchain or runtime library is needed.

    `rustc` is taken from the `RUSTC` environment variable, or else from the `PATH`.

    The launcher is configured at build time through environment variables of the `rustc` process,
    see `launcher_config()`.
    """

    # The separator of the elements of list-valued configuration variables, see launcher.rs
    LIST_SEPARATOR = '\x1f'

    # All configuration variables read by launcher.rs.
    # Those not set by `launcher_config()` are removed from the environment of `rustc`.
    CONFIG_VARIABLES = [
        'LAUNCHER_CLASS',
        'LAUNCHER_MAIN_MODULE',
        'GRAALVM_VERSION',
        'LIBJVM_RELPATH',
        'LIBJLI_RELPATH',
        'LIBLANG_RELPATH',
        'LANGUAGES_DIR',
        'TOOLS_DIR',
        'LAUNCHER_MODULE_PATH',
        'LAUNCHER_LIBRARY_PATH',
        'LAUNCHER_OPTION_VARS',
        'LAUNCHER_LANG_HOME_NAMES',
        'LAUNCHER_LANG_HOME_PATHS',
        'LAUNCHER_EXTRACTED_LIB_NAMES',
        'LAUNCHER_EXTRACTED_LIB_PATHS',
        'LAUNCHER_DEFAULT_VM_ARGS',
    ]

    def __init__(self, suite, name, deps, deliverable=None, multitarget=False, theLicense=None, **kwargs):
        """
        :param deliverable: the name of the executable, without extension. Defaults to the name of the project.
        :param multitarget: whether the executable is in a `<os>-<arch>/<libc>` subdirectory of the results,
               like for multi-target native projects. Only the host target is built.
        """
        self.deliverable = deliverable or name
        self.multitarget = multitarget
        launcher_dir = join(mx.suite('sdk').dir, 'src', 'org.graalvm.launcher.native')
        self.windows_manifest = join(launcher_dir, 'manifest', 'launcher.manifest')
        super().__init__(suite, name, 'src', [], deps, None, launcher_dir, theLicense, **kwargs)

    def isJDKDependent(self):
        # This must always be False, the compiled thin launchers need to work on both latest LTS JDK and jdk-latest
        return False

    def launcher_config(self):
        """
        Gets the build-time configuration of the launcher.

        :return: a dict from the name of a configuration variable (see CONFIG_VARIABLES)
                 to its value, a string or a list of strings.
        :rtype: dict[str, str | list[str]]
        """
        raise NotImplementedError()

    @property
    def source_file(self):
        return join(self.dir, 'src', 'launcher.rs')

    @property
    def out_dir(self):
        return join(self.get_output_base(), self.name)

    @property
    def target_subdir(self):
        # the subdirectory of the host target, like for native projects built with ninja: <os>-<arch>/<libc>
        libc = (mx.get_os_variant() or 'glibc') if mx.is_linux() else 'default'
        return join(f'{mx.get_os()}-{mx.get_arch()}', libc)

    @property
    def output_file(self):
        return join(self.out_dir, self.target_subdir, mx.exe_suffix(self.deliverable))

    def getArchivableResults(self, use_relpath=True, single=False):
        if self.multitarget:
            if single:
                raise ValueError("single not supported")
            yield self.output_file, join(self.target_subdir, basename(self.output_file))
        else:
            yield self.output_file, basename(self.output_file)

    def getBuildTask(self, args):
        return RustLauncherBuildTask(self, args, 1)

    def rustc_env(self):
        """Gets the configuration of the launcher as environment variables for `rustc`."""
        env = {}
        for name, value in self.launcher_config().items():
            if name not in RustLauncherProject.CONFIG_VARIABLES:
                self.abort(f"Unknown launcher configuration variable: {name}")
            if isinstance(value, list):
                value = [mx_subst.path_substitutions.substitute(element) for element in value]
                for element in value:
                    if RustLauncherProject.LIST_SEPARATOR in element:
                        self.abort(f"An element of {name} contains the list separator: {element!r}")
                env[name] = RustLauncherProject.LIST_SEPARATOR.join(value)
            else:
                env[name] = mx_subst.path_substitutions.substitute(str(value))
        if mx.is_darwin():
            default_min_version = {'amd64': '10.13', 'aarch64': '11.0'}[mx.get_arch()]
            env['MACOSX_DEPLOYMENT_TARGET'] = os.getenv('MACOSX_DEPLOYMENT_TARGET', default_min_version)
        return env

    def rustc_args(self):
        args = [
            '--edition', '2021',
            '--crate-name', 'launcher',
            '--crate-type', 'bin',
            '-C', 'opt-level=3',
            '-C', 'codegen-units=1',
            '-C', 'panic=abort',
            '-C', 'strip=symbols', # no debug info and no symbols to keep the launcher small
            # do not embed the absolute path of the source file
            '--remap-path-prefix', dirname(self.source_file) + '=.',
        ]
        if mx.is_windows():
            args += ['-C', 'link-arg=/MANIFEST:EMBED', '-C', 'link-arg=/MANIFESTINPUT:' + self.windows_manifest]
        return args + ['-o', self.output_file, self.source_file]


class RustLauncherBuildTask(mx.BuildTask):
    subject: RustLauncherProject

    def __str__(self):
        return f'Building {self.subject.name} with rustc'

    @staticmethod
    def rustc():
        rustc = os.environ.get('RUSTC') or 'rustc'
        if shutil.which(rustc) is None:
            mx.abort(f"Building the thin launchers requires the Rust compiler but '{rustc}' was not found.\n"
                     "Install Rust (https://www.rust-lang.org/tools/install) and ensure rustc is on the PATH, "
                     "or set the RUSTC environment variable to the path of rustc.")
        return rustc

    def witness_file(self):
        return self.subject.output_file + '.witness'

    def witness_contents(self):
        rustc = RustLauncherBuildTask.rustc()
        version = mx.OutputCapture()
        mx.run([rustc, '--version', '--verbose'], out=version)
        return json.dumps({
            'rustc': version.data.splitlines(),
            'args': self.subject.rustc_args(),
            'env': self.subject.rustc_env(),
        }, indent=2, sort_keys=True)

    def newestOutput(self):
        return mx.TimeStampFile(self.subject.output_file)

    def needsBuild(self, newestInput):
        result, reason = super().needsBuild(newestInput)
        if result:
            return result, reason
        output = self.newestOutput()
        if not output.exists():
            return True, f"{output.path} does not exist"
        for input_file in [self.subject.source_file] + ([self.subject.windows_manifest] if mx.is_windows() else []):
            if output.isOlderThan(input_file):
                return True, f"{output.path} is older than {input_file}"
        if newestInput and output.isOlderThan(newestInput):
            return True, f"{output.path} is older than {newestInput}"
        witness_file = self.witness_file()
        if not exists(witness_file):
            return True, f"{witness_file} does not exist"
        with open(witness_file, encoding='utf-8') as f:
            if f.read() != self.witness_contents():
                return True, "the compiler or the configuration of the launcher changed"
        return False, None

    def build(self):
        rustc = RustLauncherBuildTask.rustc()
        mx_util.ensure_dirname_exists(self.subject.output_file)
        env = os.environ.copy()
        for name in RustLauncherProject.CONFIG_VARIABLES:
            env.pop(name, None)
        env.update(self.subject.rustc_env())
        mx.run([rustc] + self.subject.rustc_args(), env=env)
        with open(self.witness_file(), 'w', encoding='utf-8') as f:
            f.write(self.witness_contents())

    def clean(self, forBuild=False):
        mx.rmtree(self.subject.out_dir, ignore_errors=True)
