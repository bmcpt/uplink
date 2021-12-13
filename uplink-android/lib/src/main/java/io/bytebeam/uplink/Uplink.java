// Automatically generated by flapigen
package io.bytebeam.uplink;


public final class Uplink {

    public Uplink(String device_id, String project_id, String broker, long port) {
        mNativeObj = init(device_id, project_id, broker, port);
    }
    private static native long init(String device_id, String project_id, String broker, long port);

    public final void send(String response) {
        do_send(mNativeObj, response);
    }
    private static native void do_send(long self, String response);

    public final String recv() {
        String ret = do_recv(mNativeObj);

        return ret;
    }
    private static native String do_recv(long self);

    public synchronized void delete() {
        if (mNativeObj != 0) {
            do_delete(mNativeObj);
            mNativeObj = 0;
       }
    }
    @Override
    protected void finalize() throws Throwable {
        try {
            delete();
        }
        finally {
             super.finalize();
        }
    }
    private static native void do_delete(long me);
    /*package*/ Uplink(InternalPointerMarker marker, long ptr) {
        assert marker == InternalPointerMarker.RAW_PTR;
        this.mNativeObj = ptr;
    }
    /*package*/ long mNativeObj;

        static {
            try {
                NativeUtils.loadLibraryFromJar("/libuplink_android.so"); // for macOS, make sure this is .dylib rather than .so
            } catch (java.io.IOException e) {
                e.printStackTrace();
            }
        }}