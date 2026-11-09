# Security

## Scope

multi-simu captures device and browser screens and injects input. It is a
local development tool: it talks to the simulators, emulators, and browsers on
the same machine and does not send screen contents anywhere.

The iOS adapter calls Apple's private CoreSimulator and SimulatorKit
frameworks. It is unofficial and unsupported by Apple. Use it only with
devices and simulators that you own or are authorized to control.

## Reporting

Please report security issues privately through GitHub's "Report a
vulnerability" feature on this repository, or by contacting the maintainers
listed in the repository profile. Do not open a public issue for a
vulnerability.

Include the version, the adapter (ios/android/web), and the steps to
reproduce. We will acknowledge reports as soon as possible and credit
reporters who want to be named.
