# Variable font test fixture

`variable.ttf` is an Inter variable-font subset containing `M`, `m`, and `0`,
renamed to GPUI Test Variable. It retains the `wght` axis and is licensed under
the SIL Open Font License in `OFL.txt`. It makes tests independent of system fonts.

Source: https://github.com/rsms/inter (Inter 4.001).
Generate with FontTools: subset these characters without instantiating the
variable axes, then rename name IDs 1, 3, 4, 6, and 16 to the test family.
