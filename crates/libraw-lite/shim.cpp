#include "libraw/libraw.h"

extern "C" {
void pv_set_camera_wb(libraw_data_t *lr, int v) { lr->params.use_camera_wb = v; }
int pv_exif_colorspace(libraw_data_t *lr) { return lr->color.ExifColorSpace; }
void pv_set_render(libraw_data_t *lr, int use_auto_wb, float exp_shift, float exp_preser, float bright, int no_auto_bright,
                   float auto_bright_thr, int highlight, float threshold, int fbdd_noiserd, int med_passes) {
  libraw_output_params_t &p = lr->params;
  p.use_auto_wb = use_auto_wb;
  p.exp_correc = exp_shift != 1.0f;
  p.exp_shift = exp_shift;
  p.exp_preser = exp_preser;
  p.bright = bright;
  p.no_auto_bright = no_auto_bright;
  p.auto_bright_thr = auto_bright_thr;
  p.highlight = highlight;
  p.threshold = threshold;
  p.fbdd_noiserd = fbdd_noiserd;
  p.med_passes = med_passes;
}
}
