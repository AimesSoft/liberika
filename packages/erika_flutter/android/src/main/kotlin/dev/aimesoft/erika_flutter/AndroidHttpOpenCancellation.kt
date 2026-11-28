package dev.aimesoft.erika_flutter

/** An exit invalidates queued HTTP opens and signals the one running on the owner. */
internal class AndroidHttpOpenCancellation(private val handle: Long) {
    // Read and advanced on the platform thread before queuing a command.
    private var generation = 0L
    @Volatile private var supported = true

    fun generationFor(uri: Any?): Long? =
        if (supported && uri is String && (uri.startsWith("http://") || uri.startsWith("https://"))) generation else null

    fun wasInterrupted(openGeneration: Long?): Boolean =
        supported && openGeneration != null && openGeneration != generation

    fun interrupt() {
        if (!supported) return
        generation++
        try {
            ErikaNative.nativeInterruptHttpOpen(handle, generation)
        } catch (_: UnsatisfiedLinkError) {
            supported = false
        }
    }

    // Only the owner invokes a presenter. Older native libraries use their original JNI ABI.
    fun open(argumentsJson: String, openGeneration: Long): String {
        if (supported) {
            try {
                return ErikaNative.nativeOpenHttp(handle, argumentsJson, openGeneration)
            } catch (_: UnsatisfiedLinkError) {
                supported = false
            }
        }
        return ErikaNative.nativeInvoke(handle, "open", argumentsJson, -1)
    }
}
