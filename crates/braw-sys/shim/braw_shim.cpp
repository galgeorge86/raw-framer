// Raw Framer: C shim over the Blackmagic RAW SDK (Windows / COM flavour).
//
// Each rf_braw_decode() call owns a stack Waiter. The read job and the
// decode+process job carry a pointer to it as user data; the shared codec
// callback continues the chain and finally signals the waiter. Many threads
// may decode concurrently; each waits only for its own frame.

#include <windows.h>
#include <oleauto.h>

#include "BlackmagicRawAPI.h"
#include "BlackmagicRawAPIDispatch.h"
#include "braw_shim.h"

#include <atomic>
#include <cmath>
#include <condition_variable>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <vector>

// ------------------------------------------------------------------ helpers

static thread_local std::string g_err;

static int fail(const std::string& msg, HRESULT hr = S_OK) {
    char buf[32];
    g_err = msg;
    if (hr != S_OK) {
        std::snprintf(buf, sizeof(buf), " (hr=0x%08lX)", (unsigned long)hr);
        g_err += buf;
    }
    return RF_ERR;
}

struct ComInit {
    HRESULT hr;
    ComInit() { hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED); }
    ~ComInit() { if (SUCCEEDED(hr)) CoUninitialize(); }
};
static void ensure_com() { static thread_local ComInit init; (void)init; }

template <typename T> struct Ptr {
    T* p = nullptr;
    Ptr() = default;
    explicit Ptr(T* x) : p(x) {}
    ~Ptr() { if (p) p->Release(); }
    Ptr(const Ptr&) = delete;
    Ptr& operator=(const Ptr&) = delete;
    T** out() { if (p) { p->Release(); p = nullptr; } return &p; }
    T* operator->() const { return p; }
    operator T*() const { return p; }
};

static std::string to_utf8(const wchar_t* w) {
    if (!w) return {};
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, nullptr, 0, nullptr, nullptr);
    if (n <= 1) return {};
    std::string s(n - 1, '\0');
    WideCharToMultiByte(CP_UTF8, 0, w, -1, s.data(), n, nullptr, nullptr);
    return s;
}

static std::wstring to_wide(const char* s) {
    if (!s) return {};
    int n = MultiByteToWideChar(CP_UTF8, 0, s, -1, nullptr, 0);
    if (n <= 1) return {};
    std::wstring w(n - 1, L'\0');
    MultiByteToWideChar(CP_UTF8, 0, s, -1, w.data(), n);
    return w;
}

struct Bstr {
    BSTR b = nullptr;
    Bstr() = default;
    explicit Bstr(const std::wstring& w) { b = SysAllocString(w.c_str()); }
    ~Bstr() { if (b) SysFreeString(b); }
    Bstr(const Bstr&) = delete;
    Bstr& operator=(const Bstr&) = delete;
    BSTR* out() { if (b) { SysFreeString(b); b = nullptr; } return &b; }
    std::string utf8() const { return to_utf8(b); }
};

static char* dup_string(const std::string& s) {
    char* p = (char*)std::malloc(s.size() + 1);
    if (p) std::memcpy(p, s.c_str(), s.size() + 1);
    return p;
}

// Minimal JSON writer.
struct Json {
    std::string s;
    std::vector<bool> first{true};
    void sep() { if (!first.back()) s += ','; first.back() = false; }
    void str_raw(const std::string& v) {
        s += '"';
        for (unsigned char c : v) {
            switch (c) {
                case '"': s += "\\\""; break;
                case '\\': s += "\\\\"; break;
                case '\n': s += "\\n"; break;
                case '\r': s += "\\r"; break;
                case '\t': s += "\\t"; break;
                default:
                    if (c < 0x20) { char b[8]; std::snprintf(b, sizeof(b), "\\u%04x", c); s += b; }
                    else s += (char)c;
            }
        }
        s += '"';
    }
    void key(const std::string& k) { sep(); str_raw(k); s += ':'; first.back() = true; }
    void begin_obj() { sep(); s += '{'; first.push_back(true); }
    void end_obj() { s += '}'; first.pop_back(); first.back() = false; }
    void begin_arr() { sep(); s += '['; first.push_back(true); }
    void end_arr() { s += ']'; first.pop_back(); first.back() = false; }
    void str(const std::string& v) { sep(); str_raw(v); }
    void num(double v) {
        sep();
        if (!std::isfinite(v)) { s += "null"; return; }
        char b[64];
        std::snprintf(b, sizeof(b), "%.9g", v);
        s += b;
    }
    void null() { sep(); s += "null"; }
};

static void variant_json(Json& j, VARIANT& v) {
    switch (v.vt) {
        case VT_UI1: j.num(v.bVal); break;
        case VT_I1: j.num(v.cVal); break;
        case VT_I2: j.num(v.iVal); break;
        case VT_UI2: j.num(v.uiVal); break;
        case VT_I4: j.num(v.lVal); break;
        case VT_UI4: j.num(v.ulVal); break;
        case VT_INT: j.num(v.intVal); break;
        case VT_UINT: j.num(v.uintVal); break;
        case VT_R4: j.num(v.fltVal); break;
        case VT_R8: j.num(v.dblVal); break;
        case VT_BSTR: j.str(to_utf8(v.bstrVal)); break;
        default:
            if ((v.vt & VT_ARRAY) || v.vt == VT_SAFEARRAY) {
                SAFEARRAY* sa = (v.vt & VT_BYREF) ? *v.pparray : v.parray;
                VARTYPE et = VT_EMPTY;
                LONG lo = 0, hi = -1;
                void* data = nullptr;
                if (!sa || FAILED(SafeArrayGetVartype(sa, &et)) || FAILED(SafeArrayGetLBound(sa, 1, &lo)) ||
                    FAILED(SafeArrayGetUBound(sa, 1, &hi)) || FAILED(SafeArrayAccessData(sa, &data))) {
                    j.null();
                    break;
                }
                j.begin_arr();
                LONG n = hi - lo + 1;
                if (n > 256) n = 256; // metadata arrays are tiny; LUT data is not wanted here
                for (LONG i = 0; i < n; ++i) {
                    switch (et) {
                        case VT_UI1: j.num(((uint8_t*)data)[i]); break;
                        case VT_I1: j.num(((int8_t*)data)[i]); break;
                        case VT_I2: j.num(((int16_t*)data)[i]); break;
                        case VT_UI2: j.num(((uint16_t*)data)[i]); break;
                        case VT_I4: case VT_INT: j.num(((int32_t*)data)[i]); break;
                        case VT_UI4: case VT_UINT: j.num(((uint32_t*)data)[i]); break;
                        case VT_R4: j.num(((float*)data)[i]); break;
                        case VT_R8: j.num(((double*)data)[i]); break;
                        default: j.null();
                    }
                }
                SafeArrayUnaccessData(sa);
                j.end_arr();
            } else {
                j.null();
            }
    }
}

static void metadata_json(Json& j, IBlackmagicRawMetadataIterator* it) {
    j.begin_obj();
    if (it) {
        Bstr key;
        while (SUCCEEDED(it->GetKey(key.out())) && key.b) {
            VARIANT v;
            VariantInit(&v);
            if (SUCCEEDED(it->GetData(&v))) {
                j.key(key.utf8());
                variant_json(j, v);
            }
            VariantClear(&v);
            if (it->Next() != S_OK) break;
        }
    }
    j.end_obj();
}

// ------------------------------------------------------------------ GPU read-back

// Copies a CUDA / OpenCL buffer produced by a GPU pipeline into CPU memory (blocking).
static HRESULT gpu_read(BlackmagicRawResourceType type, void* ctx, void* queue, void* res, void* dst, size_t size) {
    if (type == blackmagicRawResourceTypeBufferOpenCL) {
        // The SDK's output buffer has no host access, so copy it on the GPU into a readable
        // buffer and read that one back (blocking).
        typedef void*(__stdcall * CreateBuffer)(void*, unsigned long long, size_t, void*, int*);
        typedef int(__stdcall * CopyBuffer)(void*, void*, void*, size_t, size_t, size_t, unsigned int, const void*, void*);
        typedef int(__stdcall * ReadBuffer)(void*, void*, unsigned int, size_t, size_t, void*, unsigned int, const void*, void*);
        typedef int(__stdcall * Release)(void*);
        static HMODULE m = LoadLibraryW(L"OpenCL.dll");
        static CreateBuffer create = m ? (CreateBuffer)GetProcAddress(m, "clCreateBuffer") : nullptr;
        static CopyBuffer copyb = m ? (CopyBuffer)GetProcAddress(m, "clEnqueueCopyBuffer") : nullptr;
        static ReadBuffer read = m ? (ReadBuffer)GetProcAddress(m, "clEnqueueReadBuffer") : nullptr;
        static Release release = m ? (Release)GetProcAddress(m, "clReleaseMemObject") : nullptr;
        if (!create || !copyb || !read || !release || !ctx || !queue) return E_NOINTERFACE;
        int err = 0;
        void* tmp = create(ctx, 1 /* CL_MEM_READ_WRITE */, size, nullptr, &err);
        if (!tmp || err != 0) return E_OUTOFMEMORY;
        err = copyb(queue, res, tmp, 0, 0, size, 0, nullptr, nullptr);
        if (err == 0) err = read(queue, tmp, 1 /* CL_TRUE */, 0, size, dst, 0, nullptr, nullptr);
        release(tmp);
        return err == 0 ? S_OK : E_FAIL;
    }
    if (type == blackmagicRawResourceTypeBufferCUDA) {
        typedef int(__stdcall * CtxPush)(void*);
        typedef int(__stdcall * CtxPop)(void**);
        typedef int(__stdcall * DtoH)(void*, unsigned long long, size_t);
        static HMODULE m = LoadLibraryW(L"nvcuda.dll");
        static CtxPush push = m ? (CtxPush)GetProcAddress(m, "cuCtxPushCurrent_v2") : nullptr;
        static CtxPop pop = m ? (CtxPop)GetProcAddress(m, "cuCtxPopCurrent_v2") : nullptr;
        static DtoH copy = m ? (DtoH)GetProcAddress(m, "cuMemcpyDtoH_v2") : nullptr;
        if (!push || !pop || !copy) return E_NOINTERFACE;
        if (ctx && push(ctx) != 0) return E_FAIL;
        int err = copy(dst, (unsigned long long)(uintptr_t)res, size);
        void* prev = nullptr;
        if (ctx) pop(&prev);
        return err == 0 ? S_OK : E_FAIL;
    }
    return E_NOTIMPL;
}

// ------------------------------------------------------------------ waiter / callback

struct Waiter {
    std::mutex m;
    std::condition_variable cv;
    bool done = false;
    HRESULT hr = S_OK;
    std::string err;

    int mode = 0; // 0 = decode, 1 = frame info, 2 = set frame metadata
    const char* meta_key = nullptr;
    const char* meta_value = nullptr; // nullptr = clear (back to the camera original)
    const rf_braw_decode_t* params = nullptr;
    IBlackmagicRawClipProcessingAttributes* clipAttrs = nullptr;
    IBlackmagicRawFrameProcessingAttributes* frameAttrs = nullptr;
    IBlackmagicRaw* codec = nullptr; // for copying GPU results back to CPU memory
    rf_image_t* out = nullptr;
    std::string json;

    void finish(HRESULT r, const std::string& msg) {
        std::lock_guard<std::mutex> lk(m);
        if (done) return;
        hr = r;
        err = msg;
        done = true;
        cv.notify_all();
    }
    void wait() {
        std::unique_lock<std::mutex> lk(m);
        cv.wait(lk, [&] { return done; });
    }
};

static BlackmagicRawResolutionScale scale_of(uint32_t s) {
    switch (s) {
        case 2: return blackmagicRawResolutionScaleHalf;
        case 4: return blackmagicRawResolutionScaleQuarter;
        case 8: return blackmagicRawResolutionScaleEighth;
        default: return blackmagicRawResolutionScaleFull;
    }
}

static HRESULT set_frame_attr_u32(IBlackmagicRawFrameProcessingAttributes* fa, BlackmagicRawFrameProcessingAttribute a, uint32_t v) {
    VARIANT var; VariantInit(&var); var.vt = VT_UI4; var.ulVal = v;
    return fa->SetFrameAttribute(a, &var);
}

// Set metadata `key` to `value` (text), converted to the type the key already has (the
// SDK rejects mismatched types). A null value clears the override, restoring the camera
// original. Keys that don't exist yet are stored as numbers when they parse as one.
template <typename Get, typename Set>
static HRESULT set_metadata_typed(Get get, Set set, const char* key, const char* value) {
    Bstr k{to_wide(key)};
    if (!value) return set(k.b, nullptr);
    VARIANT cur; VariantInit(&cur);
    VARTYPE vt = VT_EMPTY;
    if (SUCCEEDED(get(k.b, &cur))) vt = cur.vt;
    VariantClear(&cur);
    VARIANT v; VariantInit(&v);
    v.vt = VT_BSTR;
    v.bstrVal = SysAllocString(to_wide(value).c_str());
    const LCID en = MAKELCID(MAKELANGID(LANG_ENGLISH, SUBLANG_ENGLISH_US), SORT_DEFAULT); // '.' decimals
    auto try_type = [&](VARTYPE t) -> HRESULT {
        VARIANT c; VariantInit(&c);
        HRESULT r = t == VT_BSTR ? VariantCopy(&c, &v) : VariantChangeTypeEx(&c, &v, en, 0, t);
        if (SUCCEEDED(r)) r = set(k.b, &c);
        VariantClear(&c);
        return r;
    };
    HRESULT hr;
    if (vt != VT_EMPTY) {
        hr = try_type(vt);
    } else {
        // New key: the SDK only accepts the type it expects, which isn't documented, so try
        // the plausible ones in turn.
        char* end = nullptr;
        std::strtod(value, &end);
        bool numeric = end && *end == '\0' && *value;
        const VARTYPE ints[] = {VT_UI4, VT_UI2, VT_UI1, VT_I4, VT_I2, VT_BOOL, VT_R4};
        const VARTYPE floats[] = {VT_R4, VT_R8};
        hr = E_INVALIDARG;
        if (!numeric) hr = try_type(VT_BSTR);
        else if (std::strchr(value, '.')) { for (VARTYPE t : floats) if (SUCCEEDED(hr = try_type(t))) break; }
        else { for (VARTYPE t : ints) if (SUCCEEDED(hr = try_type(t))) break; }
    }
    VariantClear(&v);
    return hr;
}

static void frame_info_json(Waiter* w, IBlackmagicRawFrame* frame) {
    Json j;
    j.begin_obj();
    Bstr tc;
    j.key("timecode");
    if (SUCCEEDED(frame->GetTimecode(tc.out())) && tc.b) j.str(tc.utf8()); else j.null();

    Ptr<IBlackmagicRawMetadataIterator> it;
    j.key("metadata");
    frame->GetMetadataIterator(it.out());
    metadata_json(j, it);

    Ptr<IBlackmagicRawFrameProcessingAttributes> fa;
    if (SUCCEEDED(frame->CloneFrameProcessingAttributes(fa.out())) && fa.p) {
        struct A { const char* name; BlackmagicRawFrameProcessingAttribute a; };
        const A attrs[] = {
            {"kelvin", blackmagicRawFrameProcessingAttributeWhiteBalanceKelvin},
            {"tint", blackmagicRawFrameProcessingAttributeWhiteBalanceTint},
            {"exposure", blackmagicRawFrameProcessingAttributeExposure},
            {"iso", blackmagicRawFrameProcessingAttributeISO},
            {"analog_gain", blackmagicRawFrameProcessingAttributeAnalogGain},
        };
        j.key("processing");
        j.begin_obj();
        for (auto& a : attrs) {
            VARIANT v; VariantInit(&v);
            if (SUCCEEDED(fa->GetFrameAttribute(a.a, &v))) { j.key(a.name); variant_json(j, v); }
            VariantClear(&v);
        }
        j.end_obj();
        j.key("ranges");
        j.begin_obj();
        for (auto& a : attrs) {
            VARIANT lo, hi; VariantInit(&lo); VariantInit(&hi);
            BOOL ro = FALSE;
            if (SUCCEEDED(fa->GetFrameAttributeRange(a.a, &lo, &hi, &ro))) {
                j.key(a.name);
                j.begin_arr(); variant_json(j, lo); variant_json(j, hi); j.end_arr();
            }
            VariantClear(&lo); VariantClear(&hi);
        }
        j.end_obj();
        uint32_t n = 0; BOOL ro = FALSE;
        if (SUCCEEDED(fa->GetISOList(nullptr, &n, &ro)) && n > 0) {
            std::vector<unsigned int> isos(n);
            if (SUCCEEDED(fa->GetISOList(isos.data(), &n, &ro))) {
                j.key("isos");
                j.begin_arr();
                for (uint32_t i = 0; i < n; ++i) j.num(isos[i]);
                j.end_arr();
            }
        }
    }
    j.end_obj();
    w->json = std::move(j.s);
}

class Callback : public IBlackmagicRawCallback {
public:
    void STDMETHODCALLTYPE ReadComplete(IBlackmagicRawJob* job, HRESULT result, IBlackmagicRawFrame* frame) override {
        Waiter* w = nullptr;
        job->GetUserData((void**)&w);
        if (!w) return;
        if (FAILED(result) || !frame) { w->finish(FAILED(result) ? result : E_FAIL, "reading frame failed"); return; }

        if (w->mode == 1) {
            frame_info_json(w, frame);
            w->finish(S_OK, "");
            return;
        }
        if (w->mode == 2) {
            HRESULT hr = set_metadata_typed(
                [&](BSTR k, VARIANT* v) { return frame->GetMetadata(k, v); },
                [&](BSTR k, VARIANT* v) { return frame->SetMetadata(k, v); },
                w->meta_key, w->meta_value);
            w->finish(hr, FAILED(hr) ? std::string("setting frame metadata '") + w->meta_key + "' failed" : "");
            return;
        }

        const rf_braw_decode_t* p = w->params;
        HRESULT hr = frame->SetResolutionScale(scale_of(p->scale));
        if (FAILED(hr)) { w->finish(hr, "SetResolutionScale failed"); return; }
        hr = frame->SetResourceFormat(p->format == RF_FORMAT_RGB_F32 ? blackmagicRawResourceFormatRGBF32 : blackmagicRawResourceFormatRGBAF16);
        if (FAILED(hr)) { w->finish(hr, "SetResourceFormat failed"); return; }

        IBlackmagicRawFrameProcessingAttributes* fa = nullptr;
        hr = frame->CloneFrameProcessingAttributes(&fa);
        if (FAILED(hr) || !fa) { w->finish(FAILED(hr) ? hr : E_FAIL, "CloneFrameProcessingAttributes failed"); return; }
        w->frameAttrs = fa; // released by the decode caller

        if (p->iso >= 0 && FAILED(hr = set_frame_attr_u32(fa, blackmagicRawFrameProcessingAttributeISO, (uint32_t)p->iso))) {
            w->finish(hr, "setting ISO failed"); return;
        }
        if (p->kelvin >= 0 && FAILED(hr = set_frame_attr_u32(fa, blackmagicRawFrameProcessingAttributeWhiteBalanceKelvin, (uint32_t)p->kelvin))) {
            w->finish(hr, "setting white balance failed"); return;
        }
        if (p->has_tint) {
            VARIANT v; VariantInit(&v); v.vt = VT_I2; v.iVal = (SHORT)p->tint;
            if (FAILED(hr = fa->SetFrameAttribute(blackmagicRawFrameProcessingAttributeWhiteBalanceTint, &v))) { w->finish(hr, "setting tint failed"); return; }
        }
        if (p->has_exposure) {
            VARIANT v; VariantInit(&v); v.vt = VT_R4; v.fltVal = p->exposure;
            if (FAILED(hr = fa->SetFrameAttribute(blackmagicRawFrameProcessingAttributeExposure, &v))) { w->finish(hr, "setting exposure failed"); return; }
        }

        IBlackmagicRawJob* decodeJob = nullptr;
        hr = frame->CreateJobDecodeAndProcessFrame(w->clipAttrs, fa, &decodeJob);
        if (FAILED(hr) || !decodeJob) { w->finish(FAILED(hr) ? hr : E_FAIL, "CreateJobDecodeAndProcessFrame failed"); return; }
        decodeJob->SetUserData(w);
        hr = decodeJob->Submit();
        decodeJob->Release();
        if (FAILED(hr)) w->finish(hr, "submitting decode job failed");
    }

    void STDMETHODCALLTYPE ProcessComplete(IBlackmagicRawJob* job, HRESULT result, IBlackmagicRawProcessedImage* img) override {
        Waiter* w = nullptr;
        job->GetUserData((void**)&w);
        if (!w) return;
        if (FAILED(result) || !img) { w->finish(FAILED(result) ? result : E_FAIL, "decoding/processing frame failed"); return; }
        unsigned int width = 0, height = 0, size = 0;
        void* res = nullptr;
        img->GetWidth(&width);
        img->GetHeight(&height);
        img->GetResourceSizeBytes(&size);
        HRESULT hr = img->GetResource(&res);
        if (FAILED(hr) || !res || size == 0) { w->finish(FAILED(hr) ? hr : E_FAIL, "processed image has no resource"); return; }
        BlackmagicRawResourceType type = blackmagicRawResourceTypeBufferCPU;
        img->GetResourceType(&type);
        uint8_t* copy = (uint8_t*)std::malloc(size);
        if (!copy) { w->finish(E_OUTOFMEMORY, "out of memory"); return; }
        if (type == blackmagicRawResourceTypeBufferCPU) {
            std::memcpy(copy, res, size);
        } else {
            // GPU pipeline: read the device buffer back with the driver API (loaded at run
            // time, so neither OpenCL nor CUDA is a link-time dependency).
            void* ctx = nullptr;
            void* queue = nullptr;
            img->GetResourceContextAndCommandQueue(&ctx, &queue);
            hr = gpu_read(type, ctx, queue, res, copy, size);
            if (FAILED(hr)) { std::free(copy); w->finish(hr, "copying the GPU result to memory failed"); return; }
        }
        w->out->width = width;
        w->out->height = height;
        w->out->format = w->params->format;
        w->out->data = copy;
        w->out->size = size;
        w->finish(S_OK, "");
    }

    void STDMETHODCALLTYPE ReadAudioComplete(IBlackmagicRawJob*, HRESULT, IBlackmagicRawAudioBuffer*) override {}
    void STDMETHODCALLTYPE DecodeComplete(IBlackmagicRawJob*, HRESULT) override {}
    void STDMETHODCALLTYPE TrimProgress(IBlackmagicRawJob*, float) override {}
    void STDMETHODCALLTYPE TrimComplete(IBlackmagicRawJob*, HRESULT) override {}
    void STDMETHODCALLTYPE SidecarMetadataParseWarning(IBlackmagicRawClip*, BSTR, unsigned int, BSTR) override {}
    void STDMETHODCALLTYPE SidecarMetadataParseError(IBlackmagicRawClip*, BSTR, unsigned int, BSTR) override {}
    void STDMETHODCALLTYPE PreparePipelineComplete(void*, HRESULT) override {}

    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID iid, LPVOID* out) override {
        if (!out) return E_POINTER;
        if (iid == IID_IUnknown || iid == IID_IBlackmagicRawCallback) {
            *out = static_cast<IBlackmagicRawCallback*>(this);
            AddRef();
            return S_OK;
        }
        *out = nullptr;
        return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return ++m_ref; }
    ULONG STDMETHODCALLTYPE Release() override {
        ULONG r = --m_ref;
        if (r == 0) delete this;
        return r;
    }

private:
    std::atomic<ULONG> m_ref{1};
};

// ------------------------------------------------------------------ handle

// One factory + codec for the whole process. Every codec owns its own worker-thread pool
// and memory pools, so a codec per clip (the old design) made memory and thread count grow
// with every clip opened until allocations failed. All clips are opened from this codec;
// the callback is shared and each job carries its own Waiter, so clips never interfere.
// The codec lives until the process exits.
static std::mutex g_codec_mx;
static IBlackmagicRawFactory* g_factory = nullptr;
static IBlackmagicRaw* g_codec = nullptr;
// Pipeline for the next codec (CPU, CUDA or OpenCL) and a description of the current one.
static BlackmagicRawPipeline g_want = blackmagicRawPipelineCPU;
static std::string g_active = "CPU";

static const char* pipeline_id(BlackmagicRawPipeline p) {
    switch (p) {
        case blackmagicRawPipelineCUDA: return "cuda";
        case blackmagicRawPipelineOpenCL: return "opencl";
        case blackmagicRawPipelineMetal: return "metal";
        default: return "cpu";
    }
}

// Caller holds g_codec_mx. The factory lives for the process.
static IBlackmagicRawFactory* factory_locked(const wchar_t* runtime_dir) {
    if (!g_factory) {
        if (runtime_dir && *runtime_dir) {
            Bstr dir{std::wstring(runtime_dir)};
            g_factory = CreateBlackmagicRawFactoryInstanceFromPath(dir.b);
        } else {
            g_factory = CreateBlackmagicRawFactoryInstance();
        }
    }
    return g_factory;
}

// First device of a GPU pipeline, or nullptr. `name` receives "<pipeline> · <device>".
static IBlackmagicRawPipelineDevice* first_device(IBlackmagicRawFactory* f, BlackmagicRawPipeline p, std::string* name) {
    Ptr<IBlackmagicRawPipelineDeviceIterator> it;
    if (FAILED(f->CreatePipelineDeviceIterator(p, blackmagicRawInteropNone, it.out())) || !it.p) return nullptr;
    IBlackmagicRawPipelineDevice* dev = nullptr;
    if (FAILED(it->CreateDevice(&dev)) || !dev) return nullptr;
    if (name) {
        Bstr pn, dn;
        dev->GetPipelineName(pn.out());
        dev->GetName(dn.out());
        *name = pn.utf8() + " \xC2\xB7 " + dn.utf8();
    }
    return dev;
}

static int shared_codec(const wchar_t* runtime_dir, IBlackmagicRaw** out) {
    std::lock_guard<std::mutex> lk(g_codec_mx);
    if (!g_codec) {
        IBlackmagicRawFactory* factory = factory_locked(runtime_dir);
        if (!factory) return fail("could not load BlackmagicRawAPI.dll (Blackmagic RAW runtime not found)");

        IBlackmagicRaw* codec = nullptr;
        HRESULT hr = factory->CreateCodec(&codec);
        if (FAILED(hr) || !codec) return fail("CreateCodec failed", hr);

        Ptr<IBlackmagicRawConfiguration> cfg;
        if (SUCCEEDED(codec->QueryInterface(IID_IBlackmagicRawConfiguration, (void**)cfg.out())) && cfg.p) {
            bool gpu = false;
            if (g_want != blackmagicRawPipelineCPU) {
                std::string name;
                // The device is kept alive for the process (codecs using it may outlive a switch).
                IBlackmagicRawPipelineDevice* dev = first_device(factory, g_want, &name);
                if (dev && SUCCEEDED(cfg->SetFromDevice(dev))) {
                    gpu = true;
                    g_active = name;
                } else {
                    g_active = std::string("CPU (") + pipeline_id(g_want) + " unavailable)";
                }
            }
            if (!gpu) {
                cfg->SetPipeline(blackmagicRawPipelineCPU, nullptr, nullptr);
                if (g_want == blackmagicRawPipelineCPU) g_active = "CPU";
            }
        }
        // Kept for the process lifetime (whether SetCallback adds a reference is undocumented).
        Callback* cb = new Callback();
        hr = codec->SetCallback(cb);
        if (FAILED(hr)) { codec->Release(); return fail("SetCallback failed", hr); }
        g_codec = codec;
    }
    g_codec->AddRef();
    *out = g_codec;
    return RF_OK;
}

extern "C" int rf_braw_pipelines(const wchar_t* runtime_dir, char** json_out) {
    if (!json_out) return fail("null argument");
    ensure_com();
    std::lock_guard<std::mutex> lk(g_codec_mx);
    IBlackmagicRawFactory* f = factory_locked(runtime_dir);
    if (!f) return fail("could not load BlackmagicRawAPI.dll (Blackmagic RAW runtime not found)");
    Json j;
    j.begin_arr();
    j.begin_obj(); j.key("id"); j.str("cpu"); j.key("name"); j.str("CPU"); j.end_obj();
    for (BlackmagicRawPipeline p : {blackmagicRawPipelineCUDA, blackmagicRawPipelineOpenCL}) {
        std::string name;
        IBlackmagicRawPipelineDevice* dev = first_device(f, p, &name);
        if (!dev) continue;
        dev->Release();
        j.begin_obj(); j.key("id"); j.str(pipeline_id(p)); j.key("name"); j.str(name); j.end_obj();
    }
    j.end_arr();
    *json_out = dup_string(j.s);
    return *json_out ? RF_OK : fail("out of memory");
}

extern "C" int rf_braw_set_pipeline(const char* id) {
    if (!id) return fail("null argument");
    std::string s(id);
    BlackmagicRawPipeline p = s == "cuda" ? blackmagicRawPipelineCUDA : s == "opencl" ? blackmagicRawPipelineOpenCL : blackmagicRawPipelineCPU;
    std::lock_guard<std::mutex> lk(g_codec_mx);
    if (p == g_want && g_codec) return RF_OK;
    g_want = p;
    // Clips opened from now on get a new codec; open clips keep theirs until reopened.
    if (g_codec) { g_codec->Release(); g_codec = nullptr; }
    return RF_OK;
}

extern "C" int rf_braw_active_pipeline(char** out) {
    if (!out) return fail("null argument");
    std::lock_guard<std::mutex> lk(g_codec_mx);
    *out = dup_string(g_active);
    return *out ? RF_OK : fail("out of memory");
}

struct rf_braw {
    IBlackmagicRaw* codec = nullptr; // shared; this handle holds one reference
    IBlackmagicRawClip* clip = nullptr;
    std::mutex submit;
};

// No FlushJobs here: the codec is shared, and every decode on this clip is synchronous
// and has finished by the time the last reference to the handle is dropped.
static void release_all(rf_braw* b) {
    if (b->clip) b->clip->Release();
    if (b->codec) b->codec->Release();
    delete b;
}

extern "C" int rf_braw_open(const wchar_t* clip_path, const wchar_t* runtime_dir, rf_braw** out) {
    ensure_com();
    if (!clip_path || !out) return fail("null argument");
    *out = nullptr;
    rf_braw* b = new rf_braw();
    if (shared_codec(runtime_dir, &b->codec) != RF_OK) { release_all(b); return RF_ERR; }

    Bstr path{std::wstring(clip_path)};
    HRESULT hr = b->codec->OpenClip(path.b, &b->clip);
    if (FAILED(hr) || !b->clip) { release_all(b); return fail("could not open clip", hr); }

    *out = b;
    return RF_OK;
}

extern "C" void rf_braw_close(rf_braw* b) {
    if (!b) return;
    ensure_com();
    release_all(b);
}

extern "C" int rf_braw_info(rf_braw* b, rf_braw_info_t* out) {
    if (!b || !out) return fail("null argument");
    ensure_com();
    unsigned int w = 0, h = 0;
    float fps = 0;
    unsigned long long n = 0;
    HRESULT hr;
    if (FAILED(hr = b->clip->GetWidth(&w)) || FAILED(hr = b->clip->GetHeight(&h)) ||
        FAILED(hr = b->clip->GetFrameRate(&fps)) || FAILED(hr = b->clip->GetFrameCount(&n)))
        return fail("reading clip info failed", hr);
    out->width = w;
    out->height = h;
    out->fps = fps;
    out->frame_count = n;
    return RF_OK;
}

static void list_json(Json& j, IBlackmagicRawClipProcessingAttributes* ca, BlackmagicRawClipProcessingAttribute a) {
    unsigned int n = 0;
    BOOL ro = FALSE;
    j.begin_arr();
    if (SUCCEEDED(ca->GetClipAttributeList(a, nullptr, &n, &ro)) && n > 0 && n < 4096) {
        std::vector<VARIANT> vals(n);
        for (auto& v : vals) VariantInit(&v);
        if (SUCCEEDED(ca->GetClipAttributeList(a, vals.data(), &n, &ro))) {
            for (unsigned int i = 0; i < n; ++i) variant_json(j, vals[i]);
        }
        for (auto& v : vals) VariantClear(&v);
    }
    j.end_arr();
}

static HRESULT set_u16(IBlackmagicRawClipProcessingAttributes* ca, BlackmagicRawClipProcessingAttribute a, uint16_t v) {
    VARIANT var; VariantInit(&var); var.vt = VT_UI2; var.uiVal = v;
    return ca->SetClipAttribute(a, &var);
}

static HRESULT set_str(IBlackmagicRawClipProcessingAttributes* ca, BlackmagicRawClipProcessingAttribute a, const char* s) {
    VARIANT var; VariantInit(&var); var.vt = VT_BSTR; var.bstrVal = SysAllocString(to_wide(s).c_str());
    HRESULT hr = ca->SetClipAttribute(a, &var);
    VariantClear(&var);
    return hr;
}

extern "C" int rf_braw_clip_json(rf_braw* b, char** json_out) {
    if (!b || !json_out) return fail("null argument");
    ensure_com();
    Json j;
    j.begin_obj();

    Ptr<IBlackmagicRawConfiguration> cfg;
    j.key("version");
    Bstr ver;
    if (SUCCEEDED(b->codec->QueryInterface(IID_IBlackmagicRawConfiguration, (void**)cfg.out())) && cfg.p &&
        SUCCEEDED(cfg->GetVersion(ver.out())) && ver.b) j.str(ver.utf8()); else j.null();

    Bstr cam;
    j.key("camera_type");
    if (SUCCEEDED(b->clip->GetCameraType(cam.out())) && cam.b) j.str(cam.utf8()); else j.null();

    Bstr tc;
    j.key("timecode0");
    if (SUCCEEDED(b->clip->GetTimecodeForFrame(0, tc.out())) && tc.b) j.str(tc.utf8()); else j.null();

    Ptr<IBlackmagicRawMetadataIterator> it;
    j.key("metadata");
    b->clip->GetMetadataIterator(it.out());
    metadata_json(j, it);

    Ptr<IBlackmagicRawClipProcessingAttributes> ca;
    if (SUCCEEDED(b->clip->CloneClipProcessingAttributes(ca.out())) && ca.p) {
        struct A { const char* name; BlackmagicRawClipProcessingAttribute a; };
        const A attrs[] = {
            {"color_science_gen", blackmagicRawClipProcessingAttributeColorScienceGen},
            {"gamma", blackmagicRawClipProcessingAttributeGamma},
            {"gamut", blackmagicRawClipProcessingAttributeGamut},
            {"highlight_recovery", blackmagicRawClipProcessingAttributeHighlightRecovery},
            {"gamut_compression", blackmagicRawClipProcessingAttributeGamutCompressionEnable},
            {"post3dlut_mode", blackmagicRawClipProcessingAttributePost3DLUTMode},
            {"analog_gain", blackmagicRawClipProcessingAttributeAnalogGain},
            {"tone_curve_contrast", blackmagicRawClipProcessingAttributeToneCurveContrast},
            {"embedded_lut_name", blackmagicRawClipProcessingAttributeEmbeddedPost3DLUTName},
        };
        j.key("processing");
        j.begin_obj();
        for (auto& a : attrs) {
            VARIANT v; VariantInit(&v);
            if (SUCCEEDED(ca->GetClipAttribute(a.a, &v))) { j.key(a.name); variant_json(j, v); }
            VariantClear(&v);
        }
        j.end_obj();

        j.key("lists");
        j.begin_obj();
        j.key("color_science_gen"); list_json(j, ca, blackmagicRawClipProcessingAttributeColorScienceGen);
        j.key("gamma"); list_json(j, ca, blackmagicRawClipProcessingAttributeGamma);
        j.key("gamut"); list_json(j, ca, blackmagicRawClipProcessingAttributeGamut);
        j.key("post3dlut_mode"); list_json(j, ca, blackmagicRawClipProcessingAttributePost3DLUTMode);
        unsigned int n = 0; BOOL ro = FALSE;
        j.key("iso");
        j.begin_arr();
        if (SUCCEEDED(ca->GetISOList(nullptr, &n, &ro)) && n > 0 && n < 4096) {
            std::vector<unsigned int> isos(n);
            if (SUCCEEDED(ca->GetISOList(isos.data(), &n, &ro)))
                for (unsigned int i = 0; i < n; ++i) j.num(isos[i]);
        }
        j.end_arr();
        j.end_obj();
    }
    j.end_obj();
    *json_out = dup_string(j.s);
    return *json_out ? RF_OK : fail("out of memory");
}

extern "C" int rf_braw_gamma_list(rf_braw* b, int32_t gen, char** json_out) {
    if (!b || !json_out) return fail("null argument");
    ensure_com();
    Ptr<IBlackmagicRawClipProcessingAttributes> ca;
    HRESULT hr = b->clip->CloneClipProcessingAttributes(ca.out());
    if (FAILED(hr) || !ca.p) return fail("CloneClipProcessingAttributes failed", hr);
    if (gen >= 0 && FAILED(hr = set_u16(ca, blackmagicRawClipProcessingAttributeColorScienceGen, (uint16_t)gen)))
        return fail("color science generation not supported", hr);
    Json j;
    list_json(j, ca, blackmagicRawClipProcessingAttributeGamma);
    *json_out = dup_string(j.s);
    return *json_out ? RF_OK : fail("out of memory");
}

static int submit_read(rf_braw* b, uint64_t frame, Waiter& w) {
    IBlackmagicRawJob* job = nullptr;
    HRESULT hr;
    {
        std::lock_guard<std::mutex> lk(b->submit);
        hr = b->clip->CreateJobReadFrame(frame, &job);
        if (FAILED(hr) || !job) return fail("CreateJobReadFrame failed", hr);
        job->SetUserData(&w);
        hr = job->Submit();
        job->Release();
    }
    if (FAILED(hr)) return fail("submitting read job failed", hr);
    w.wait();
    return RF_OK;
}

extern "C" int rf_braw_frame_json(rf_braw* b, uint64_t frame, char** json_out) {
    if (!b || !json_out) return fail("null argument");
    ensure_com();
    Waiter w;
    w.mode = 1;
    if (submit_read(b, frame, w) != RF_OK) return RF_ERR;
    if (FAILED(w.hr)) return fail(w.err, w.hr);
    *json_out = dup_string(w.json);
    return *json_out ? RF_OK : fail("out of memory");
}

extern "C" int rf_braw_set_metadata(rf_braw* b, int64_t frame, const char* key, const char* value) {
    if (!b || !key) return fail("null argument");
    ensure_com();
    if (frame < 0) {
        HRESULT hr = set_metadata_typed(
            [&](BSTR k, VARIANT* v) { return b->clip->GetMetadata(k, v); },
            [&](BSTR k, VARIANT* v) { return b->clip->SetMetadata(k, v); },
            key, value);
        return FAILED(hr) ? fail(std::string("setting clip metadata '") + key + "' failed", hr) : RF_OK;
    }
    Waiter w;
    w.mode = 2;
    w.meta_key = key;
    w.meta_value = value;
    if (submit_read(b, (uint64_t)frame, w) != RF_OK) return RF_ERR;
    if (FAILED(w.hr)) return fail(w.err, w.hr);
    return RF_OK;
}

extern "C" int rf_braw_save_sidecar(rf_braw* b) {
    if (!b) return fail("null argument");
    ensure_com();
    HRESULT hr = b->clip->SaveSidecarFile();
    return FAILED(hr) ? fail("saving the .sidecar file failed", hr) : RF_OK;
}

extern "C" int rf_braw_reload_sidecar(rf_braw* b) {
    if (!b) return fail("null argument");
    ensure_com();
    HRESULT hr = b->clip->ReloadSidecarFile();
    return FAILED(hr) ? fail("reloading the .sidecar file failed", hr) : RF_OK;
}

extern "C" int rf_braw_sidecar_attached(rf_braw* b, int* attached) {
    if (!b || !attached) return fail("null argument");
    ensure_com();
    BOOL a = FALSE;
    HRESULT hr = b->clip->GetSidecarFileAttached(&a);
    *attached = a ? 1 : 0;
    return FAILED(hr) ? fail("querying the .sidecar file failed", hr) : RF_OK;
}

extern "C" int rf_braw_decode(rf_braw* b, const rf_braw_decode_t* p, rf_image_t* out) {
    if (!b || !p || !out) return fail("null argument");
    ensure_com();
    std::memset(out, 0, sizeof(*out));

    Ptr<IBlackmagicRawClipProcessingAttributes> ca;
    HRESULT hr = b->clip->CloneClipProcessingAttributes(ca.out());
    if (FAILED(hr) || !ca.p) return fail("CloneClipProcessingAttributes failed", hr);
    // Order matters: generation decides which gamuts/gammas are valid.
    if (p->color_science_gen >= 0 && FAILED(hr = set_u16(ca, blackmagicRawClipProcessingAttributeColorScienceGen, (uint16_t)p->color_science_gen)))
        return fail("setting color science generation failed", hr);
    if (p->gamut && FAILED(hr = set_str(ca, blackmagicRawClipProcessingAttributeGamut, p->gamut)))
        return fail(std::string("setting gamut '") + p->gamut + "' failed", hr);
    if (p->gamma && FAILED(hr = set_str(ca, blackmagicRawClipProcessingAttributeGamma, p->gamma)))
        return fail(std::string("setting gamma '") + p->gamma + "' failed", hr);
    if (p->highlight_recovery >= 0 && FAILED(hr = set_u16(ca, blackmagicRawClipProcessingAttributeHighlightRecovery, (uint16_t)p->highlight_recovery)))
        return fail("setting highlight recovery failed", hr);
    if (p->gamut_compression >= 0 && FAILED(hr = set_u16(ca, blackmagicRawClipProcessingAttributeGamutCompressionEnable, (uint16_t)p->gamut_compression)))
        return fail("setting gamut compression failed", hr);
    if (p->post3dlut_mode && FAILED(hr = set_str(ca, blackmagicRawClipProcessingAttributePost3DLUTMode, p->post3dlut_mode)))
        return fail("setting 3D LUT mode failed", hr);

    Waiter w;
    w.mode = 0;
    w.params = p;
    w.codec = b->codec;
    w.clipAttrs = ca.p;
    w.out = out;
    int rc = submit_read(b, p->frame, w);
    if (w.frameAttrs) w.frameAttrs->Release();
    if (rc != RF_OK) return rc;
    if (FAILED(w.hr)) {
        if (out->data) { std::free(out->data); out->data = nullptr; }
        return fail(w.err, w.hr);
    }
    return RF_OK;
}

// ------------------------------------------------------------------ audio

static IBlackmagicRawClipAudio* clip_audio(rf_braw* b) {
    IBlackmagicRawClipAudio* a = nullptr;
    if (FAILED(b->clip->QueryInterface(IID_IBlackmagicRawClipAudio, (void**)&a))) return nullptr;
    return a;
}

extern "C" int rf_braw_audio_info(rf_braw* b, rf_braw_audio_info_t* out) {
    if (!b || !out) return fail("null argument");
    ensure_com();
    Ptr<IBlackmagicRawClipAudio> a(clip_audio(b));
    if (!a.p) return fail("clip has no audio");
    unsigned int rate = 0, ch = 0, bits = 0;
    unsigned long long n = 0;
    HRESULT hr;
    if (FAILED(hr = a->GetAudioSampleRate(&rate)) || FAILED(hr = a->GetAudioChannelCount(&ch)) ||
        FAILED(hr = a->GetAudioBitDepth(&bits)) || FAILED(hr = a->GetAudioSampleCount(&n)))
        return fail("reading audio format failed", hr);
    if (rate == 0 || ch == 0 || n == 0) return fail("clip has no audio");
    out->sample_rate = rate;
    out->channels = ch;
    out->bit_depth = bits;
    out->sample_count = n;
    return RF_OK;
}

extern "C" int rf_braw_audio_read(rf_braw* b, uint64_t start, uint32_t max_frames, void* buf, uint32_t buf_bytes, uint32_t* frames_read) {
    if (!b || !buf || !frames_read) return fail("null argument");
    ensure_com();
    *frames_read = 0;
    Ptr<IBlackmagicRawClipAudio> a(clip_audio(b));
    if (!a.p) return fail("clip has no audio");
    unsigned int got = 0, bytes = 0;
    HRESULT hr = a->GetAudioSamples((long long)start, buf, buf_bytes, max_frames, &got, &bytes);
    if (FAILED(hr)) return fail("reading audio samples failed", hr);
    *frames_read = got;
    return RF_OK;
}

extern "C" void rf_free(void* p) { std::free(p); }

extern "C" const char* rf_braw_last_error(void) { return g_err.c_str(); }
