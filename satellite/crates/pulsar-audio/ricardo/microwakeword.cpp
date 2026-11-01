// Ricardo on-device wake word ("Рикардо") for the Rust daemon.
//
// This is the v0.4.1 C++ detector ported behind a small C ABI. It uses the
// firmware's Yandex TFLite runtime (libtflite.so, loaded with dlopen) and the
// vendored TensorFlow Lite microfrontend, so the trained model sees exactly the
// features it was trained on. The model and the runtime are never distributed
// in this repository.
//
// Only the audio processing thread calls mww_feed/mww_reset/mww_configure.
// mww_destroy runs after that thread has stopped.

#include <dlfcn.h>

#include <algorithm>
#include <array>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <new>
#include <string>

#include "tensorflow/lite/experimental/microfrontend/lib/frontend.h"
#include "tensorflow/lite/experimental/microfrontend/lib/frontend_util.h"

namespace {
using P = void *;
struct Quantization {
  float scale;
  int32_t zero;
};

// The thresholds come from the model manifest (see wake_events::logic): the detector
// defaults match the trained model when it has none.
constexpr float kCutoff = 0.9f;
constexpr size_t kWindow = 10;
// Upper bound of a configurable sliding window (contract v1 allows 1..50).
constexpr size_t kMaxWindow = 50;
constexpr const char *kLibTflite = "/system/vendor/quasar/libs/libtflite.so";

void log_error(const char *what) { std::fprintf(stderr, "mww: %s\n", what); }

struct Impl {
  P lib = nullptr, model = nullptr, interpreter = nullptr, resolver = nullptr;
  P (*model_create)(const char *) = nullptr;
  void (*model_delete)(P) = nullptr;
  P (*options_create)() = nullptr;
  void (*options_threads)(P, int32_t) = nullptr;
  void (*options_delete)(P) = nullptr;
  P (*resolver_create)(const char *) = nullptr;
  P (*interpreter_create)(P, P, P) = nullptr;
  void (*interpreter_delete)(P) = nullptr;
  int (*allocate)(P) = nullptr;
  P (*input)(P, int32_t) = nullptr;
  P (*output)(P, int32_t) = nullptr;
  int32_t (*num_dims)(P) = nullptr;
  int32_t (*dim)(P, int32_t) = nullptr;
  int (*type)(P) = nullptr;
  size_t (*bytes)(P) = nullptr;
  Quantization (*quant)(P) = nullptr;
  int (*copy_in)(P, const void *, size_t) = nullptr;
  int (*copy_out)(P, void *, size_t) = nullptr;
  int (*invoke)(P) = nullptr;

  FrontendState fe{};
  bool fe_ready = false, ready = false;
  std::array<int8_t, 120> features{};
  std::array<float, kMaxWindow> probs{};
  float cutoff = kCutoff;
  size_t window = kWindow;
  size_t filled = 0, count = 0, pos = 0;
  int warmup = 100;
  Quantization iq{}, oq{};

  template <class F>
  bool bind(F &f, const char *name) {
    f = reinterpret_cast<F>(dlsym(lib, name));
    if (!f) {
      std::fprintf(stderr, "mww: missing %s\n", name);
      return false;
    }
    return true;
  }

  bool open(const std::string &path) {
    lib = dlopen(kLibTflite, RTLD_NOW | RTLD_GLOBAL);
    if (!lib) {
      std::fprintf(stderr, "mww: libtflite: %s\n", dlerror());
      return false;
    }
#define B(member, symbol) \
  if (!bind(member, symbol)) return false
    B(model_create, "TfLiteModelCreateFromFile");
    B(model_delete, "TfLiteModelDelete");
    B(options_create, "TfLiteInterpreterOptionsCreate");
    B(options_threads, "TfLiteInterpreterOptionsSetNumThreads");
    B(options_delete, "TfLiteInterpreterOptionsDelete");
    B(resolver_create, "TfLiteCustomOpResolverCreate");
    B(interpreter_create, "TfLiteInterpreterCreateWithCustomResolver");
    B(interpreter_delete, "TfLiteInterpreterDelete");
    B(allocate, "TfLiteInterpreterAllocateTensors");
    B(input, "TfLiteInterpreterGetInputTensor");
    B(output, "TfLiteInterpreterGetOutputTensor");
    B(num_dims, "TfLiteTensorNumDims");
    B(dim, "TfLiteTensorDim");
    B(type, "TfLiteTensorType");
    B(bytes, "TfLiteTensorByteSize");
    B(quant, "TfLiteTensorQuantizationParams");
    B(copy_in, "TfLiteTensorCopyFromBuffer");
    B(copy_out, "TfLiteTensorCopyToBuffer");
    B(invoke, "TfLiteInterpreterInvoke");
#undef B
    model = model_create(path.c_str());
    // Yandex's default interpreter resolver crashes; its named FULL resolver is
    // required. Keep it loaded for the firmware's other users.
    static P full_resolver = nullptr;
    if (!full_resolver) full_resolver = resolver_create("FULL");
    resolver = full_resolver;
    if (!model || !resolver) {
      log_error("model or FULL resolver unavailable");
      return false;
    }

    FrontendConfig cfg{};
    FrontendFillConfigWithDefaults(&cfg);
    cfg.window.size_ms = 30;
    cfg.window.step_size_ms = 10;
    cfg.filterbank.num_channels = 40;
    cfg.filterbank.lower_band_limit = 125;
    cfg.filterbank.upper_band_limit = 7500;
    cfg.noise_reduction.smoothing_bits = 10;
    cfg.noise_reduction.even_smoothing = 0.025f;
    cfg.noise_reduction.odd_smoothing = 0.06f;
    cfg.noise_reduction.min_signal_remaining = 0.05f;
    cfg.pcan_gain_control.enable_pcan = 1;
    cfg.pcan_gain_control.strength = 0.95f;
    cfg.pcan_gain_control.offset = 80;
    cfg.pcan_gain_control.gain_bits = 21;
    cfg.log_scale.enable_log = 1;
    cfg.log_scale.scale_shift = 6;
    if (!FrontendPopulateState(&cfg, &fe, 16000)) return false;
    fe_ready = true;
    if (!reset()) return false;
    std::fprintf(
        stderr,
        "mww: loaded %s (int8 [1,3,40], cutoff=%.2f, window=%zu)\n",
        path.c_str(),
        cutoff,
        window);
    return true;
  }

  // Sets the manifest thresholds; call before feeding. The history restarts so the
  // windowed mean never mixes two window sizes.
  void configure(float c, size_t w) {
    if (c > 0 && c < 1) cutoff = c;
    if (w >= 1 && w <= kMaxWindow) window = w;
    probs.fill(0);
    count = pos = 0;
  }

  bool reset() {
    ready = false;
    if (interpreter) interpreter_delete(interpreter);
    interpreter = nullptr;
    P options = options_create();
    if (!options) return false;
    options_threads(options, 1);
    interpreter = interpreter_create(model, resolver, options);
    options_delete(options);
    if (!interpreter || allocate(interpreter) != 0) {
      log_error("interpreter creation/allocation failed");
      return false;
    }
    P in = input(interpreter, 0), out = output(interpreter, 0);
    if (!in || !out || num_dims(in) != 3 || dim(in, 0) != 1 || dim(in, 1) != 3 ||
        dim(in, 2) != 40 || type(in) != 9 || type(out) != 3 ||
        bytes(in) != features.size() || bytes(out) != 1) {
      log_error("expected int8 [1,3,40] -> uint8 [1,1]");
      return false;
    }
    iq = quant(in);
    oq = quant(out);
    if (!(iq.scale > 0) || !(oq.scale > 0)) return false;
    FrontendReset(&fe);
    filled = count = pos = 0;
    warmup = 100;
    probs.fill(0);
    ready = true;
    return true;
  }

  ~Impl() {
    if (interpreter && interpreter_delete) interpreter_delete(interpreter);
    if (model && model_delete) model_delete(model);
    if (fe_ready) FrontendFreeStateContents(&fe);
    // Keep the named resolver and library loaded for the firmware's other users.
  }

  bool feed(const int16_t *pcm, size_t samples, float &score) {
    score = -1;
    if (!ready) return false;
    bool hit = false;
    while (samples) {
      size_t consumed = 0;
      auto frame = FrontendProcessSamples(&fe, pcm, samples, &consumed);
      pcm += consumed;
      samples -= consumed;
      if (frame.size == 40) {
        for (size_t i = 0; i < 40; ++i) {
          // pymicro_features uses the microfrontend's fixed-point output * 0.0390625.
          long q = std::lrint(frame.values[i] * 0.0390625f / iq.scale) + iq.zero;
          features[filled * 40 + i] = static_cast<int8_t>(std::clamp(q, -128L, 127L));
        }
        if (++filled == 3) {
          filled = 0;
          uint8_t result = 0;
          if (copy_in(input(interpreter, 0), features.data(), features.size()) != 0 ||
              invoke(interpreter) != 0 ||
              copy_out(output(interpreter, 0), &result, 1) != 0) {
            log_error("inference failed");
            ready = false;
            return false;
          }
          probs[pos++ % window] = (int(result) - oq.zero) * oq.scale;
          count = std::min(count + 1, window);
          if (warmup > 0) {
            warmup -= 3;
            continue;
          }
          if (count == window) {
            float mean = 0;
            for (size_t i = 0; i < window; ++i) mean += probs[i] / window;
            // Also reported without a hit: near-miss recording needs the running mean
            // of every chunk (contract v1).
            score = std::max(score, mean);
            hit = hit || mean >= cutoff;
          }
        }
      }
      if (!consumed) break;
    }
    if (hit) reset();  // fresh streaming variables, gain estimates and a 1 s warm-up
    return hit;
  }
};

}  // namespace

extern "C" {

// NULL on error; the reason is logged to stderr.
void *mww_create(const char *model_path) {
  if (!model_path || !*model_path) return nullptr;
  auto *impl = new (std::nothrow) Impl;
  if (!impl) return nullptr;
  if (!impl->open(model_path)) {
    delete impl;
    return nullptr;
  }
  return impl;
}

void mww_destroy(void *handle) { delete static_cast<Impl *>(handle); }

// 1 = detected, 0 = no detection, -1 = detector failed and was disabled.
int mww_feed(void *handle, const int16_t *pcm, size_t samples, float *score) {
  if (!handle) return -1;
  float s = -1;
  bool hit = static_cast<Impl *>(handle)->feed(pcm, samples, s);
  if (score) *score = s;
  if (!static_cast<Impl *>(handle)->ready && !hit) return -1;
  return hit ? 1 : 0;
}

int mww_reset(void *handle) {
  if (!handle) return -1;
  return static_cast<Impl *>(handle)->reset() ? 0 : -1;
}

// Manifest thresholds from the wake-event contract: cutoff in (0,1), window 1..50.
// Values outside the ranges are ignored (the loaded defaults stay).
void mww_configure(void *handle, float cutoff, int window) {
  if (!handle || window <= 0) return;
  static_cast<Impl *>(handle)->configure(cutoff, static_cast<size_t>(window));
}

}  // extern "C"
