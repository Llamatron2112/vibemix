/* Test-only helper: parse an atom Sequence with the real LV2 iterators and
   return the `time:beatsPerMinute` of the first `time:Position` object. */

#include <stdint.h>

#include <lv2/atom/atom.h>
#include <lv2/atom/util.h>

float vibemix_test_position_bpm(const void* buffer, uint32_t urid_object,
                                uint32_t urid_position, uint32_t urid_float,
                                uint32_t urid_bpm) {
  const LV2_Atom_Sequence* seq = (const LV2_Atom_Sequence*)buffer;
  for (LV2_Atom_Event* ev = lv2_atom_sequence_begin(&seq->body);
       !lv2_atom_sequence_is_end(&seq->body, seq->atom.size, ev);
       ev = lv2_atom_sequence_next(ev)) {
    if (ev->body.type != urid_object) {
      continue;
    }
    const LV2_Atom_Object* obj = (const LV2_Atom_Object*)&ev->body;
    if (obj->body.otype != urid_position) {
      continue;
    }
    LV2_ATOM_OBJECT_FOREACH(obj, prop) {
      if (prop->key == urid_bpm && prop->value.type == urid_float) {
        return ((const LV2_Atom_Float*)&prop->value)->body;
      }
    }
  }
  return -1.0f;
}
