"""Run with KiCad's bundled Python; all fixtures are private or in memory."""
import unittest
import pcbnew as p
import reference_text_closed as worker


class ImmutableReferenceTests(unittest.TestCase):
    def test_serialized_guard_preserves_unknown_reference_properties(self):
        def board(field):
            return ('(kicad_pcb\n (footprint "R"\n' + field + '\n )\n)').encode()
        before = board('(property "Reference" "R1" (at 1 2) (unlocked yes) '
                       '(effects (font (size 1 1) (line_spacing 0))))')
        geometry_only = board('(property "Reference" "R1" (at 3 4 90) (unlocked yes) '
                              '(effects (font (size 0.8 0.8) (thickness 0.15) (line_spacing 0))))')
        for changed in [geometry_only.replace(b"unlocked yes", b"unlocked no"),
                        geometry_only.replace(b"line_spacing 0", b"line_spacing 1")]:
            with self.subTest(changed=changed):
                self.assertNotEqual(worker.reference_immutable_snapshot(before, {"R1"}),
                                    worker.reference_immutable_snapshot(changed, {"R1"}))
        self.assertEqual(worker.reference_immutable_snapshot(before, {"R1"}),
                         worker.reference_immutable_snapshot(geometry_only, {"R1"}))

    def test_native_immutable_flags_are_detected(self):
        board = p.BOARD()
        footprint = p.FOOTPRINT(board)
        footprint.SetReference("R1")
        field = footprint.Reference()
        for getter, setter in [
            (field.IsKeepUpright, field.SetKeepUpright),
            (field.IsKnockout, field.SetIsKnockout),
            (field.IsLocked, field.SetLocked),
            (field.IsMultilineAllowed, field.SetMultilineAllowed),
            (field.IsForceVisible, field.SetForceVisible),
        ]:
            with self.subTest(getter=getter.__name__):
                before = worker.field_snapshot(field)
                setter(not getter())
                self.assertNotEqual(before, worker.field_snapshot(field))


if __name__ == "__main__":
    unittest.main()
