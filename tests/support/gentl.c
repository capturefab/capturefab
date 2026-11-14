/* MIT-licensed Capturefab test producer. Independently implements the small
 * GenTL C ABI subset from EMVA GenTL1.6; no vendor/EMVA source is embedded. */
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <fcntl.h>
#include <sys/file.h>
#ifndef MODE
#define MODE 0
#endif
#define JOB_PART_MODE (MODE >= 192 && MODE <= 198)
#define ACTION_KEY_MODE (MODE == 199 || MODE == 200)
#define EXPORT __attribute__((visibility("default")))
typedef void *H;
typedef struct { unsigned char *data; size_t size; int queued; } Buffer;
static int tl, iface, dev, port, stream, event;
static int interfaces[16];
static int running, acquiring;
static uint64_t frame;
static Buffer *buffers[32];
static unsigned int count;
static unsigned int acqreg;
#if ACTION_KEY_MODE
static unsigned char action_key[4];
#endif
static uint32_t lockreg, transferreg;
#if JOB_PART_MODE
/* IEEE754 binary64 1000.0, explicitly little-endian register bytes. */
static unsigned char job_exposure[8] = {0, 0, 0, 0, 0, 0x40, 0x8f, 0x40};
#endif
#define STREAM_SIZE_MODE (MODE >= 105 && MODE <= 109)
#define PACKED_BICOLOR_MODE (MODE >= 180 && MODE <= 191)
#define PACKED_BICOLOR_10 (MODE == 180 || MODE == 181 || MODE == 184 || MODE == 186 || MODE == 187 || MODE == 189 || MODE == 191)
#define PACKED_BICOLOR_CODE (PACKED_BICOLOR_10 ? (MODE == 181 ? UINT32_C(0x021400aa) : UINT32_C(0x021400a8)) : (MODE == 183 || MODE == 185 ? UINT32_C(0x021800ae) : UINT32_C(0x021800ac)))
#define PACKED_BICOLOR_CHUNKS (MODE == 184 || MODE == 185)
#define PACKED_BICOLOR_WIDTH (MODE == 187 || MODE == 190 ? 1 : MODE == 186 ? 3 : PACKED_BICOLOR_10 ? 4 : 3)
#define PACKED_BICOLOR_HEIGHT (MODE == 187 || MODE == 190 ? 1 : 2)
static size_t packed_bicolor_image_end(void);
#define BICOLOR_MODE (MODE >= 169 && MODE <= 179)
#define BICOLOR_BITS ((MODE == 169 || MODE == 170 || MODE == 179) ? 8 : (MODE == 171 || MODE == 172) ? 10 : 12)
#define BICOLOR_CODE (MODE == 169 || MODE == 179 ? UINT32_C(0x021000a5) : MODE == 170 ? UINT32_C(0x021000a6) : MODE == 171 ? UINT32_C(0x022000a7) : MODE == 172 ? UINT32_C(0x022000a9) : MODE == 174 ? UINT32_C(0x022000ad) : UINT32_C(0x022000ab))
#define BICOLOR_CHUNKS (MODE == 178)
#define BICOLOR_BIG (MODE == 171 || MODE == 173 || MODE == 178 || MODE == 179)
static size_t bicolor_image_end(void);
#define IDS_BAYER_MODE (MODE >= 140 && MODE <= 168)
#define IDS_GR_MODE (MODE == 150 || MODE == 151 || MODE == 156 || MODE == 157 || (MODE >= 162 && MODE <= 168))
#define IDS_GB_MODE (MODE == 152 || MODE == 153 || MODE == 158 || MODE == 159)
#define IDS_BG_MODE (MODE == 154 || MODE == 155 || MODE == 160 || MODE == 161)
#define IDS_MODE ((MODE >= 126 && MODE <= 139) || IDS_BAYER_MODE)
#define IDS_12 (MODE == 127 || MODE == 137 || MODE == 139 || MODE == 141 || MODE == 143 || MODE == 145 || MODE == 151 || MODE == 153 || MODE == 155 || MODE == 157 || MODE == 159 || MODE == 161 || MODE == 167)
#define IDS_CHUNKS (MODE == 136 || MODE == 137 || MODE == 142 || MODE == 143 || (MODE >= 156 && MODE <= 161))
#define IDS_CODE (IDS_GR_MODE ? (IDS_12 ? UINT32_C(0x40000013) : UINT32_C(0x40000003)) : IDS_GB_MODE ? (IDS_12 ? UINT32_C(0x40000012) : UINT32_C(0x40000002)) : IDS_BG_MODE ? (IDS_12 ? UINT32_C(0x40000014) : UINT32_C(0x40000004)) : IDS_BAYER_MODE ? (IDS_12 ? UINT32_C(0x40000011) : UINT32_C(0x40000001)) : (IDS_12 ? UINT32_C(0x4000001f) : UINT32_C(0x4000000f)))
static size_t ids_image_end(void);
#if IDS_MODE
#define MOCK_XML_VENDOR "IDS Imaging Development Systems GmbH"
#else
#define MOCK_XML_VENDOR "Capturefab"
#endif
#if IDS_GR_MODE
#if IDS_12
#define IDS_XML_CODE "0x40000013"
#define IDS_XML_NAME "BayerGR12g24IDS"
#else
#define IDS_XML_CODE "0x40000003"
#define IDS_XML_NAME "BayerGR10g40IDS"
#endif
#elif IDS_GB_MODE
#if IDS_12
#define IDS_XML_CODE "0x40000012"
#define IDS_XML_NAME "BayerGB12g24IDS"
#else
#define IDS_XML_CODE "0x40000002"
#define IDS_XML_NAME "BayerGB10g40IDS"
#endif
#elif IDS_BG_MODE
#if IDS_12
#define IDS_XML_CODE "0x40000014"
#define IDS_XML_NAME "BayerBG12g24IDS"
#else
#define IDS_XML_CODE "0x40000004"
#define IDS_XML_NAME "BayerBG10g40IDS"
#endif
#elif IDS_BAYER_MODE
#if IDS_12
#define IDS_XML_CODE "0x40000011"
#define IDS_XML_NAME "BayerRG12g24IDS"
#else
#define IDS_XML_CODE "0x40000001"
#define IDS_XML_NAME "BayerRG10g40IDS"
#endif
#elif IDS_12
#define IDS_XML_CODE "0x4000001f"
#define IDS_XML_NAME "Mono12g24IDS"
#elif MODE == 130
#define IDS_XML_CODE "0x40000010"
#define IDS_XML_NAME "Mono10g40IDS"
#elif MODE == 129
#define IDS_XML_CODE "0x4000000f"
#define IDS_XML_NAME "WrongIDSLayout"
#else
#define IDS_XML_CODE "0x4000000f"
#define IDS_XML_NAME "Mono10g40IDS"
#endif
#define REGISTER_MODE (MODE >= 121 && MODE <= 125)
#if MODE == 125
#define REGISTER_SELECTOR "LineSelector"
#else
#define REGISTER_SELECTOR "LUTSelector"
#endif
#define REGISTER_BYTES (MODE == 124 ? 65536 : 16384)
static unsigned char raw_register[REGISTER_BYTES];
static uint32_t register_bank;
static int lifecycle_mode(void) { return MODE == 56 || MODE == 75 || STREAM_SIZE_MODE; }
static unsigned int waits;
static int producer_lock=-1;
static const char *interface_id="if/0;?test";
static const char *device_id="device:0/test";
static const char *xml=
"<RegisterDescription ModelName=\"Mock\" VendorName=\"" MOCK_XML_VENDOR "\" StandardNameSpace=\"None\" SchemaMajorVersion=\"1\" SchemaMinorVersion=\"1\" SchemaSubMinorVersion=\"0\">"
"<Category Name=\"Root\"><pFeature>Width</pFeature><pFeature>Height</pFeature>"
#if MODE != 105
"<pFeature>PayloadSize</pFeature>"
#endif
"<pFeature>AcquisitionStart</pFeature><pFeature>AcquisitionStop</pFeature><pFeature>ChunkCounterSample</pFeature>"
#if IDS_MODE
"<pFeature>PixelFormat</pFeature><pFeature>ExposureTime</pFeature>"
#elif JOB_PART_MODE
"<pFeature>ExposureTime</pFeature>"
#endif
#if MODE == 56 || MODE == 75 || STREAM_SIZE_MODE
"<pFeature>TLParamsLocked</pFeature>"
#endif
#if MODE == 75
"<pFeature>TransferControlMode</pFeature><pFeature>TransferStart</pFeature><pFeature>TransferStop</pFeature>"
#endif
#if REGISTER_MODE
"<pFeature>LUTValueAll</pFeature><pFeature>" REGISTER_SELECTOR "</pFeature><pFeature>HugeBuffer</pFeature>"
#endif
#if ACTION_KEY_MODE
"<pFeature>ActionDeviceKey</pFeature>"
#endif
"</Category>"
#if IDS_MODE
#if IDS_BAYER_MODE
"<Integer Name=\"Width\"><Value>8</Value></Integer><Integer Name=\"Height\"><Value>4</Value></Integer>"
#else
"<Integer Name=\"Width\"><Value>4</Value></Integer><Integer Name=\"Height\"><Value>2</Value></Integer>"
#endif
"<Float Name=\"ExposureTime\"><Value>1000</Value><Min>1</Min><Max>1000000</Max><Unit>us</Unit></Float>"
"<Enumeration Name=\"PixelFormat\"><EnumEntry Name=\"" IDS_XML_NAME "\">"
#if MODE == 147 || MODE == 163
"<Symbolic>DifferentBayerLayout</Symbolic>"
#endif
#if MODE == 131
"<pValue>DynamicPixelCode</pValue>"
#else
"<Value>" IDS_XML_CODE "</Value>"
#endif
"</EnumEntry><Value>" IDS_XML_CODE "</Value></Enumeration>"
#if MODE == 131
"<Integer Name=\"DynamicPixelCode\"><Value>" IDS_XML_CODE "</Value></Integer>"
#endif
#elif PACKED_BICOLOR_MODE
#if MODE == 187 || MODE == 190
"<Integer Name=\"Width\"><Value>1</Value></Integer><Integer Name=\"Height\"><Value>1</Value></Integer>"
#elif PACKED_BICOLOR_10 && MODE != 186
"<Integer Name=\"Width\"><Value>4</Value></Integer><Integer Name=\"Height\"><Value>2</Value></Integer>"
#else
"<Integer Name=\"Width\"><Value>3</Value></Integer><Integer Name=\"Height\"><Value>2</Value></Integer>"
#endif
#else
"<Integer Name=\"Width\"><Value>3</Value></Integer><Integer Name=\"Height\"><Value>2</Value></Integer>"
#endif
#if JOB_PART_MODE
"<Float Name=\"ExposureTime\"><pValue>JobExposureReg</pValue><Min>1</Min><Max>1000000</Max><Unit>us</Unit></Float>"
"<FloatReg Name=\"JobExposureReg\"><Address>0x118</Address><Length>8</Length><AccessMode>RW</AccessMode><pPort>Device</pPort><Endianess>LittleEndian</Endianess></FloatReg>"
#endif
#if STREAM_SIZE_MODE && MODE != 105
"<Integer Name=\"PayloadSize\"><pValue>PayloadReg</pValue></Integer>"
"<IntReg Name=\"PayloadReg\"><Address>0x110</Address><Length>4</Length><AccessMode>RO</AccessMode><pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"
#elif IDS_BAYER_MODE
"<Integer Name=\"PayloadSize\"><Value>128</Value></Integer>"
#elif MODE != 105
"<Integer Name=\"PayloadSize\"><Value>64</Value></Integer>"
#endif
"<Command Name=\"AcquisitionStart\"><pValue>AcqReg</pValue><CommandValue>1</CommandValue></Command>"
"<Command Name=\"AcquisitionStop\"><pValue>AcqReg</pValue><CommandValue>0</CommandValue></Command>"
"<IntReg Name=\"AcqReg\"><Address>0x100</Address><Length>4</Length><AccessMode>RW</AccessMode><pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"
#if MODE == 56 || MODE == 75 || STREAM_SIZE_MODE
"<Integer Name=\"TLParamsLocked\"><pValue>LockReg</pValue><Min>0</Min><Max>1</Max></Integer>"
"<IntReg Name=\"LockReg\"><Address>0x104</Address><Length>4</Length><AccessMode>RW</AccessMode><pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"
#endif
#if MODE == 75
"<Enumeration Name=\"TransferControlMode\"><EnumEntry Name=\"UserControlled\"><Value>2</Value></EnumEntry><pValue>TransferModeReg</pValue></Enumeration>"
"<IntReg Name=\"TransferModeReg\"><Address>0x10c</Address><Length>4</Length><AccessMode>RO</AccessMode><pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"
"<Command Name=\"TransferStart\"><pValue>TransferReg</pValue><CommandValue>1</CommandValue></Command>"
"<Command Name=\"TransferStop\"><pValue>TransferReg</pValue><CommandValue>0</CommandValue></Command>"
"<IntReg Name=\"TransferReg\"><Address>0x108</Address><Length>4</Length><AccessMode>RW</AccessMode><pPort>Device</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"
#endif
"<Integer Name=\"ChunkCounterSample\"><pValue>ChunkCounterReg</pValue><Unit>ticks</Unit></Integer>"
"<IntReg Name=\"ChunkCounterReg\"><Address>0</Address><Length>8</Length><AccessMode>RO</AccessMode><pPort>ChunkPort</pPort><Sign>Unsigned</Sign><Endianess>LittleEndian</Endianess></IntReg>"
"<Port Name=\"ChunkPort\"><ChunkID>0000000100000001</ChunkID></Port>"
#if REGISTER_MODE
"<Enumeration Name=\"" REGISTER_SELECTOR "\"><EnumEntry Name=\"LUT0\"><Value>0</Value></EnumEntry><EnumEntry Name=\"LUT1\"><Value>1</Value></EnumEntry><pValue>LUTBankReg</pValue><pSelected>LUTValueAll</pSelected></Enumeration>"
"<IntReg Name=\"LUTBankReg\"><Address>0x200</Address><Length>4</Length><AccessMode>RW</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"
"<Integer Name=\"LUTLength\"><pValue>LUTLengthReg</pValue></Integer>"
"<IntReg Name=\"LUTLengthReg\"><Address>0x204</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"
"<IntSwissKnife Name=\"LUTOffset\"><pVariable Name=\"B\">" REGISTER_SELECTOR "</pVariable><Formula>B * 8192</Formula></IntSwissKnife>"
"<Register Name=\"LUTValueAll\"><Address>0x8000</Address><pAddress>LUTOffset</pAddress><pLength>LUTLength</pLength><AccessMode>RW</AccessMode><pPort>Device</pPort></Register>"
"<Register Name=\"HugeBuffer\"><Address>0x100000</Address><Length>1048576</Length><AccessMode>RW</AccessMode><pPort>Device</pPort></Register>"
#endif

#if ACTION_KEY_MODE
"<Integer Name=\"ActionDeviceKey\"><AccessMode>WO</AccessMode><pValue>ActionKeyReg</pValue><Min>0</Min><Max>0xffffffff</Max><Inc>1</Inc><pIsAvailable>ActionReady</pIsAvailable><pError>ActionError</pError></Integer>"
"<IntReg Name=\"ActionKeyReg\"><Address>0x11000</Address><Length>4</Length><AccessMode>WO</AccessMode><Sign>Unsigned</Sign><Endianess>BigEndian</Endianess><pPort>Device</pPort></IntReg>"
"<IntReg Name=\"ActionReady\"><Address>0x11020</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"
"<IntReg Name=\"ActionError\"><Address>0x11010</Address><Length>4</Length><AccessMode>RO</AccessMode><Endianess>LittleEndian</Endianess></IntReg>"
#endif
"<Port Name=\"Device\"/></RegisterDescription>";
static void trace(const char *name) { const char *path=getenv("CAPTUREFAB_GENTL_MOCK_LOG"); if(path){ FILE *f=fopen(path,"a"); if(f){fprintf(f,"%ld %s\n",(long)getpid(),name);fclose(f);} } }
__attribute__((constructor)) static void loaded(void) { trace("load"); fputs("native constructor stdout must be isolated\n",stdout); fflush(stdout); }
static int text(const char *value, char *out, size_t *size) { size_t needed=strlen(value)+1; if(MODE==9)needed=1000000; if(!out||*size<needed){*size=needed;return -1016;} memcpy(out,value,needed);*size=needed;return 0; }
static int info(void *out,size_t *size,int *type,int kind,const void *value,size_t needed) { *type=kind;if(!out||*size<needed){*size=needed;return -1016;}memcpy(out,value,needed);*size=needed;return 0; }
#define INFO(kind,type,val) do { type v=(val); return info(out,size,datatype,(kind),&v,sizeof(v)); } while(0)
EXPORT int GCInitLib(void) { trace("init"); if(REGISTER_MODE){const unsigned char pattern[3]={0,128,255};for(size_t i=0;i<sizeof(raw_register);i++)raw_register[i]=pattern[i%3];} if(MODE==36||MODE==56||MODE==75||JOB_PART_MODE||ACTION_KEY_MODE){const char *path=getenv("CAPTUREFAB_GENTL_MOCK_LOCK");if(!path)return -1010;producer_lock=open(path,O_CREAT|O_RDWR,0600);if(producer_lock<0)return -1010;if(flock(producer_lock,LOCK_EX|LOCK_NB)){trace("producer_lock_busy");close(producer_lock);producer_lock=-1;return -1004;}trace("producer_locked");} if(MODE==13){const char *file=getenv("CAPTUREFAB_GENTL_MOCK_XML_FILE");FILE *f=file?fopen(file,"wb"):NULL;if(!f)return -1010;fwrite(xml,1,strlen(xml),f);fclose(f);} if(MODE==35){usleep(400000);}if(MODE==2){sleep(30);} if(MODE==3){trace("crash");abort();} for(int i=0;i<3000;i++) fputs("native verbose log\n",stdout);fflush(stdout); return 0; }
EXPORT int GCCloseLib(void) {
    trace("close_lib");
    if (lifecycle_mode()) {
        if (running || acquiring || lockreg || transferreg) {
            trace("stale_camera_state");
        }
        for (unsigned i = 0; i < count; i++) {
            if (buffers[i]) {
                trace("stale_buffer");
            }
        }
    }
    if ((MODE == 36 || lifecycle_mode() || ACTION_KEY_MODE) && producer_lock >= 0) {
        usleep(MODE == 36 ? 200000 : 20000);
        flock(producer_lock, LOCK_UN);
        close(producer_lock);
        producer_lock = -1;
        trace("producer_unlocked");
    }
    if (MODE == 31 || MODE == 35) sleep(30);
    if (MODE == 32) abort();
    return 0;
}
EXPORT int TLOpen(H *h) {trace("tl_open");*h=&tl;return 0;}
EXPORT int TLClose(H h) {(void)h;trace("tl_close");return 0;}
static unsigned interface_count(void){return MODE==30?16:(MODE==28||MODE==29)?3:1;}
static void enumeration_wait(const char *phase,uint64_t timeout){if(MODE==28||MODE==29||MODE==30){char name[80];snprintf(name,sizeof(name),"%s_wait_ms=%llu",phase,(unsigned long long)timeout);trace(name);usleep((useconds_t)(timeout*1000));}}
EXPORT int TLUpdateInterfaceList(H h,uint8_t *changed,uint64_t timeout) {(void)h;enumeration_wait("tl",timeout);*changed=1;return 0;}
EXPORT int TLGetNumInterfaces(H h,uint32_t *n) {(void)h;*n=interface_count();return 0;}
EXPORT int TLGetInterfaceID(H h,uint32_t i,char *out,size_t *size) {(void)h;if(i>=interface_count())return -1017;if(i+1==interface_count())return text(interface_id,out,size);char name[32];snprintf(name,sizeof(name),"empty/%u",i);return text(name,out,size);}
EXPORT int TLOpenInterface(H h,const char *id,H *out) {(void)h;unsigned index=interface_count()-1;if(strcmp(id,interface_id)){char name[32];for(index=0;index+1<interface_count();index++){snprintf(name,sizeof(name),"empty/%u",index);if(!strcmp(id,name))break;}if(index+1>=interface_count())return -1007;}trace("if_open");if(interface_count()>1){interfaces[index]=(int)index;*out=&interfaces[index];}else *out=&iface;return 0;}
EXPORT int IFClose(H h) {(void)h;trace("if_close");return 0;}
EXPORT int IFUpdateDeviceList(H h,uint8_t *changed,uint64_t timeout) {enumeration_wait("if",timeout);if(MODE==29&&h==&interfaces[0])return -1005;*changed=1;return 0;}
EXPORT int IFGetNumDevices(H h,uint32_t *n) {*n=interface_count()>1&&h!=&interfaces[interface_count()-1]?0:1;return 0;}
EXPORT int IFGetDeviceID(H h,uint32_t i,char *out,size_t *size) {(void)h;if(i)return -1017;return text(device_id,out,size);}
EXPORT int IFGetDeviceInfo(H h,const char *id,int cmd,int *datatype,void *out,size_t *size) {(void)h;if(strcmp(id,device_id))return -1007;*datatype=MODE==16&&cmd==7?8:1;const char *v=cmd==1?(IDS_MODE&&MODE!=128&&MODE!=146&&MODE!=162?"IDS Imaging Development Systems GmbH":"Capturefab"):cmd==2?"Mock CTI camera":cmd==7?"MOCK-001":device_id;return text(v,out,size);}
EXPORT int IFOpenDevice(H h,const char *id,int access,H *out) {(void)h;if(access!=3||strcmp(id,device_id))return -1005;trace("dev_open");*out=&dev;return 0;}
#if MODE != 12 && MODE != 25
EXPORT int DevGetInfo(H h,int cmd,int *datatype,void *out,size_t *size) {(void)h;if(cmd!=9)return -1003;if(MODE==21){INFO(8,uint64_t,1000000);}if(MODE==22){INFO(8,uint64_t,0);}if(MODE==23){INFO(5,uint64_t,1000000);}if(MODE==24){uint64_t value=1000000;return info(out,size,datatype,8,&value,7);}return -1003;}
#endif
EXPORT int DevClose(H h) {(void)h;trace("dev_close");return 0;}
EXPORT int DevGetPort(H h,H *out) {(void)h;*out=&port;return 0;}
EXPORT int DevGetNumDataStreams(H h,uint32_t *n) {(void)h;*n=1;return 0;}
EXPORT int DevGetDataStreamID(H h,uint32_t i,char *out,size_t *size) {(void)h;if(i)return -1017;return text("stream/0",out,size);}
EXPORT int DevOpenDataStream(H h,const char *id,H *out) {(void)h;if(strcmp(id,"stream/0"))return -1007;trace("ds_open");*out=&stream;return 0;}
EXPORT int GCReadPort(H h, uint64_t address, void *out, size_t *size) {
    (void)h;
    trace("read");
#if ACTION_KEY_MODE
    if (address < UINT64_C(0x11004) && (address >= UINT64_C(0x11000) || *size > UINT64_C(0x11000) - address)) {
        trace("forbidden_action_key_read");
        return -1005;
    }
    if (address == 0x11020 && *size == 4) {
        const unsigned char ready[4] = {1,0,0,0};
        memcpy(out,ready,sizeof(ready));
        trace("action_key_ready_read");
        return 0;
    }
    if (address == 0x11010 && *size == 4) {
        memset(out,0,4);
        trace("action_key_error_read");
        return 0;
    }
#endif
#if JOB_PART_MODE
    if (address == 0x118 && *size == sizeof(job_exposure)) {
        memcpy(out, job_exposure, sizeof(job_exposure));
        trace("job_exposure_read");
        return 0;
    }
#endif
    if (REGISTER_MODE && address == 0x200 && *size == 4) { memcpy(out,&register_bank,4);trace("register_bank_read");return 0; }
    if (REGISTER_MODE && address == 0x204 && *size == 4) { const uint32_t length=MODE==124?65536:8192;memcpy(out,&length,4);trace("register_length");return 0; }
    if (REGISTER_MODE && address >= 0x8000 && address - 0x8000 <= sizeof(raw_register) && *size <= sizeof(raw_register) - (size_t)(address - 0x8000)) {
        trace("register_body_read");if(MODE==122)usleep(2000000);memcpy(out,raw_register+(size_t)(address-0x8000),*size);return 0;
    }
    if (*size == 4) {
        if (address == 0x100) {
            memcpy(out, &acqreg, 4);
            return 0;
        }
        if (lifecycle_mode() && address == 0x104) {
            memcpy(out, &lockreg, 4);
            return 0;
        }
        if (MODE == 75 && address == 0x108) {
            memcpy(out, &transferreg, 4);
            return 0;
        }
        if (STREAM_SIZE_MODE && address == 0x110) {
            const unsigned char payload_size[4] = {32, 0, 0, 0};
            trace("remote_payload_size");
            if (!lockreg) return -1005;
            memcpy(out, payload_size, sizeof(payload_size));
            return 0;
        }
        if (MODE == 75 && address == 0x10c) {
            const uint32_t user_controlled = 2;
            memcpy(out, &user_controlled, 4);
            return 0;
        }
    }
    if (address >= 0x1000 && address - 0x1000 + *size <= strlen(xml)) {
        memcpy(out, xml + (address - 0x1000), *size);
        return 0;
    }
    return -1015;
}
EXPORT int GCWritePort(H h, uint64_t address, const void *in, size_t *size) {
    (void)h;
#if ACTION_KEY_MODE
    if (address == 0x11000 && *size == 4) {
        const unsigned char one[4] = {0x11,0x22,0x33,0x44};
        const unsigned char two[4] = {0xaa,0xbb,0xcc,0xdd};
        const unsigned char denied[4] = {1,2,3,4};
        const unsigned char short_write[4] = {0x10,0x20,0x30,0x40};
        const unsigned char after[4] = {0x55,0x66,0x77,0x88};
        const unsigned char late[4] = {0xde,0xad,0xbe,0xef};
        trace("action_key_write");
        if (!memcmp(in,denied,4)) {
            action_key[0] = ((const unsigned char *)in)[0];
            trace("action_key_denied");
            return -1005;
        }
        if (!memcmp(in,short_write,4)) {
            action_key[0] = ((const unsigned char *)in)[0];
            *size = 1;
            trace("action_key_short");
            return 0;
        }
        if (!memcmp(in,late,4)) {
            action_key[0] = ((const unsigned char *)in)[0];
            trace("action_key_late");
            usleep(2000000);
        } else if (!memcmp(in,one,4)) trace("action_key_bytes_one");
        else if (!memcmp(in,two,4)) trace("action_key_bytes_two");
        else if (!memcmp(in,after,4)) trace("action_key_bytes_after");
        else { trace("wrong_action_key_bytes"); return -1009; }
        memcpy(action_key,in,4);
        return 0;
    }
#endif
#if JOB_PART_MODE
    if (address == 0x118 && *size == sizeof(job_exposure)) {
        memcpy(job_exposure, in, sizeof(job_exposure));
        trace("job_exposure_write");
        return 0;
    }
#endif
    if (REGISTER_MODE && address == 0x200 && *size == 4) { memcpy(&register_bank,in,4);trace(register_bank?"register_bank_one":"register_bank_zero");return 0; }
    if (REGISTER_MODE && address >= 0x8000 && address - 0x8000 <= sizeof(raw_register) && *size <= sizeof(raw_register) - (size_t)(address - 0x8000)) {
        trace("register_body_write");if(MODE==123)usleep(2000000);memcpy(raw_register+(size_t)(address-0x8000),in,*size);return 0;
    }
    if (*size != 4) return -1015;
    if (address == 0x100) {
#if MODE == 200
        uint32_t requested;
        memcpy(&requested, in, 4);
        if (requested && memcmp(action_key, "\x11\x22\x33\x44", 4)) {
            trace("industrial_acquire_before_key");
            return -1005;
        }
#endif
        memcpy(&acqreg, in, 4);
        acquiring = acqreg != 0;
        trace(acquiring ? "acq_start" : "acq_stop");
        return 0;
    }
    if (lifecycle_mode() && address == 0x104) {
        memcpy(&lockreg, in, 4);
        trace(lockreg ? "tl_lock" : "tl_unlock");
        return 0;
    }
    if (MODE == 75 && address == 0x108) {
        memcpy(&transferreg, in, 4);
        trace(transferreg ? "transfer_start" : "transfer_stop");
        return 0;
    }
    return -1015;
}
EXPORT int GCGetPortURL(H h,char *out,size_t *size) {(void)h;if(MODE==13){const char *file=getenv("CAPTUREFAB_GENTL_MOCK_XML_URL");return file?text(file,out,size):-1010;}char url[128];snprintf(url,sizeof(url),"local:mock.xml;1000;%zx",strlen(xml));if(MODE==33||MODE==34){char list[512];size_t needed=0;if(MODE==34){const char *first="https://vendor.invalid/unavailable.xml";needed=strlen(first)+1;memcpy(list,first,needed);}size_t length=strlen(url)+1;memcpy(list+needed,url,length);needed+=length;list[needed++]=0;if(!out||*size<needed){*size=needed;return -1016;}memcpy(out,list,needed);*size=needed;return 0;}return text(url,out,size);}
static int gendc_mode(void) { return (MODE >= 84 && MODE <= 99) || (MODE >= 101 && MODE <= 104) || MODE == 120; }
EXPORT int DSGetInfo(H h,int cmd,int *datatype,void *out,size_t *size) {
    (void)h;
    if (cmd == 9) {
        if (STREAM_SIZE_MODE) {
            trace("stream_defines_payload_size");
            if (!lockreg) return -1005;
        }
        if (MODE == 12 || MODE == 109) return -1003;
        INFO(11, uint8_t, MODE != 106);
    }
    if (cmd == 7) {
        if (STREAM_SIZE_MODE) {
            trace("stream_payload_size");
            if (!lockreg || MODE == 106 || MODE == 109) return -1005;
        }
        if (MODE == 107) { INFO(12, size_t, 128 * 1024 * 1024 + 1); }
        if (MODE == 108) { INFO(8, size_t, 64); }
        INFO(12, size_t, gendc_mode() ? 512 : IDS_BAYER_MODE ? 128 : 64);
    }
    if (cmd == 12) { INFO(12, size_t, MODE == 10 ? 33 : 2); }
    return -1003;
}
EXPORT int DSAllocAndAnnounceBuffer(H h,size_t size,void *private,H *out) {
    (void)h;
    (void)private;
    if (STREAM_SIZE_MODE && (!lockreg || size != (MODE == 105 ? 64 : 32) || MODE == 107 || MODE == 108)) {
        trace("unexpected_alloc");
        return -1009;
    }
    unsigned slot=0;while(slot<32&&buffers[slot])slot++;if(slot==32)return -1020;Buffer *b=calloc(1,sizeof(*b));if(!b)return -1020;b->data=malloc(size);b->size=size;buffers[slot]=b;if(slot>=count)count=slot+1;*out=b;trace("alloc");return 0;}
EXPORT int DSQueueBuffer(H h,H handle) {(void)h;Buffer *b=(Buffer*)handle;memset(b->data,0xee,b->size);b->queued=1;trace("queue");return 0;}
EXPORT int DSFlushQueue(H h,int flags) {(void)h;(void)flags;for(unsigned i=0;i<count;i++)if(buffers[i])buffers[i]->queued=0;trace("flush");return 0;}
EXPORT int DSStartAcquisition(H h,int flags,uint64_t n) {(void)h;(void)flags;(void)n;
#if MODE == 200
    if (memcmp(action_key, "\x11\x22\x33\x44", 4)) {
        trace("industrial_stream_before_key");
        return -1005;
    }
#endif
    running=1;trace("ds_start");return 0;}
EXPORT int DSStopAcquisition(H h,int flags) {(void)h;(void)flags;running=0;trace("ds_stop");return 0;}
EXPORT int DSRevokeBuffer(H h,H handle,void **base,void **private) {(void)h;Buffer *b=(Buffer*)handle;if(b->queued)return -1004;*base=b->data;*private=NULL;for(unsigned i=0;i<count;i++)if(buffers[i]==b)buffers[i]=NULL;free(b->data);free(b);trace("revoke");return 0;}
EXPORT int DSClose(H h) {(void)h;trace("ds_close");return 0;}
EXPORT int GCRegisterEvent(H h,int kind,H *out) {(void)h;if(kind!=1)return -1009;*out=&event;trace("event_register");return 0;}
EXPORT int GCUnregisterEvent(H h,int kind) {(void)h;(void)kind;trace("event_unregister");return 0;}
static int chunk_mode(void){return MODE>=37&&MODE<=53;}
static void chunk_payload(Buffer *b){const unsigned char raw[]={0x91,0x92,0x93,0x94,1,2,3,4,5,6,0xa1,0xa2,7,8,9,10,11,12,0xa3,0xa4,0xa5,0xa6,255,255,255,255,255,255,255,255,0xc1,0xc2};memcpy(b->data,raw,sizeof(raw));}
#pragma pack(push,1)
typedef struct { uint64_t id; ptrdiff_t offset; size_t length; } ChunkInfo;
#pragma pack(pop)
#if MODE != 38
EXPORT int DSGetBufferChunkData(H h,H buffer,ChunkInfo *out,size_t *count){(void)h;(void)buffer;trace("chunk_descriptors");if(PACKED_BICOLOR_CHUNKS){if(!out){*count=1;return 0;}if(*count<1){*count=1;return -1016;}ChunkInfo chunk={UINT64_C(0x100000001),(ptrdiff_t)packed_bicolor_image_end(),8};memcpy(out,&chunk,sizeof(chunk));*count=1;return 0;}if(BICOLOR_CHUNKS){if(!out){*count=1;return 0;}if(*count<1){*count=1;return -1016;}ChunkInfo chunk={UINT64_C(0x100000001),(ptrdiff_t)bicolor_image_end(),8};memcpy(out,&chunk,sizeof(chunk));*count=1;return 0;}if(IDS_CHUNKS){if(!out){*count=1;return 0;}if(*count<1){*count=1;return -1016;}ChunkInfo chunk={UINT64_C(0x100000001),(ptrdiff_t)ids_image_end(),8};memcpy(out,&chunk,sizeof(chunk));*count=1;return 0;}if(!chunk_mode()||MODE==39||MODE==47)return -1008;size_t needed=MODE==40?4097:MODE==52?0:3;if(!out){*count=needed;return 0;}if(*count<needed){*count=needed;return -1016;}ChunkInfo chunks[3]={{UINT64_MAX,0,4},{UINT64_C(0x100000001),22,8},{UINT64_C(0x200000002),30,2}};if(MODE==41)chunks[0].offset=-1;if(MODE==42)chunks[0].length=SIZE_MAX;if(MODE==43)chunks[0].offset=40;if(MODE==49)chunks[2].id=UINT64_C(0x100000001);if(MODE==50){chunks[0].offset=4;chunks[0].length=18;}memcpy(out,chunks,sizeof(chunks));*count=MODE==44?4:MODE==53?2:3;return 0;}
#endif
static int packed_range_mode(void) { return MODE >= 110 && MODE <= 114; }
static int abc16_multipart_mode(void) { return MODE >= 116 && MODE <= 119; }
static int job_part_mode(void) { return JOB_PART_MODE; }
static int multipart_mode(void) { return job_part_mode() || (MODE >= 54 && MODE <= 83) || packed_range_mode() || abc16_multipart_mode(); }
static int point_cloud_mode(void) { return (MODE >= 80 && MODE <= 83) || abc16_multipart_mode(); }
static unsigned multipart_count(void) { return job_part_mode() ? (MODE == 198 ? 2 : 5) : MODE == 82 ? 2 : 3; }
static int scalar_mode(void) { return MODE >= 76 && MODE <= 79; }
static int multipart_two_images(void){return MODE==56||MODE==62||MODE==75;}
static int multipart_wide(void){return MODE==55||MODE==66;}
static size_t multipart_length(unsigned index) {
    if (job_part_mode()) {
        if (MODE == 198) return index == 0 ? 2 : 1;
        if (index == 0) return MODE == 193 ? 6 : 2;
        if (index == 1) return MODE == 193 ? 12 : MODE == 196 && frame >= 2 ? 3 : 4;
        return index == 2 ? 2 : 1;
    }
    if (packed_range_mode()) return index == 0 ? (MODE == 112 ? 8 : 10) : index == 1 ? 4 : 6;
    if (point_cloud_mode()) return index == 0 ? (abc16_multipart_mode() ? 18 : 36) : 6;
    if (scalar_mode() || index == 0) return 12;
    if (index == 1) return multipart_wide() ? 12 : 6;
    return multipart_two_images() ? 6 : 2;
}
static size_t multipart_offset(unsigned index) {
    if (job_part_mode()) {
        const size_t offsets[] = {4,16,40,48,56};
        return MODE == 198 ? (index == 0 ? 40 : 48) : offsets[index];
    }
    if (index == 0) return 0;
    if (point_cloud_mode()) return index == 1 && MODE != 82 ? 40 : 56;
    return index == 1 ? 24 : MODE == 62 ? 28 : 48;
}
static size_t multipart_filled(void) {
    size_t bytes = MODE == 63 ? 1 : 0;
    for (unsigned i = 0; i < multipart_count(); i++) bytes += multipart_length(i);
    return bytes;
}
static void point_cloud_word(unsigned char *out, uint32_t word, unsigned bytes) {
    for (unsigned i = 0; i < bytes; i++) {
        unsigned position = (MODE == 81 || MODE == 115 || MODE == 117) ? bytes - 1 - i : i;
        out[i] = (unsigned char)(word >> (8 * position));
    }
}
static void point_cloud_payload(Buffer *buffer) {
    const uint32_t words[9] = {
        0x3f800000, 0x80000000, 0x7fc01234,
        0x7f800001, 0xff800000, 0xc0200000,
        0, 0, 0,
    };
    const uint16_t intensity[3] = {10, 20, 30};
    const uint16_t confidence[3] = {0xffff, 0x1234, 0};
    if (abc16_multipart_mode()) {
        const uint16_t samples[9] = {0, 1, 0xffff, 0x8000, 0xfffe, 0x1234, 7, 8, 9};
        for (unsigned i = 0; i < 9; i++) point_cloud_word(buffer->data + 2 * i, samples[i], 2);
    } else {
        for (unsigned i = 0; i < 9; i++) point_cloud_word(buffer->data + 4 * i, words[i], 4);
    }
    if (MODE != 82) {
        unsigned char *out = buffer->data + multipart_offset(1);
        for (unsigned i = 0; i < 3; i++) point_cloud_word(out + 2 * i, intensity[i], 2);
    }
    unsigned char *out = buffer->data + multipart_offset(MODE == 82 ? 1 : 2);
    for (unsigned i = 0; i < 3; i++) point_cloud_word(out + 2 * i, confidence[i], 2);
}
static void scalar_payload(Buffer *buffer) {
    const uint16_t samples[3][6] = {
        {0, 1, 0x7fff, 0x8000, 0xfffe, 0xffff},
        {10, 20, 30, 40, 50, 60},
        {0xffff, 0x4321, 0x8001, 0x00ff, 0x0100, 0},
    };
    for (unsigned part = 0; part < 3; part++) {
        unsigned char *out = buffer->data + multipart_offset(part);
        for (unsigned i = 0; i < 6; i++) {
            uint16_t value = samples[part][i];
            out[2*i] = (unsigned char)(MODE == 77 ? value >> 8 : value);
            out[2*i+1] = (unsigned char)(MODE == 77 ? value : value >> 8);
        }
    }
}
/* Hand-authored standard8-bit images plus aBE C16 word and opaque customError byte.
 * These metadata/identity values are spec-derived fixtures, not observed Basler buffers. */
static int job_part_payload(Buffer *b) {
    static const unsigned char bytes[5][12] = {
        {10,20,30,40,50,60},
        {10,20,30,40,50,60,130,140,150,160,170,180},
        {0x12,0x34}, {200}, {0xab}
    };
    for (unsigned i = 0; i < multipart_count(); i++) {
        size_t offset = multipart_offset(i), length = multipart_length(i);
        if (offset > b->size || length > b->size - offset) { trace("job_parts_exceed_announced_buffer"); return -1009; }
        const unsigned char *source = MODE == 198 ? bytes[i + 2] : bytes[i];
        memcpy(b->data + offset, source, length);
    }
    trace("job_parts_payload");
    return 0;
}
static void multipart_payload(Buffer *b){
    if (packed_range_mode()) {
        const unsigned char c10[] = {0x00, 0x04, 0x50, 0xd5, 0xff, 0x01, 0x8e, 0xa4, 0xaa, 0xff};
        const unsigned char mono[] = {11, 12, 13, 14};
        const unsigned char c12[] = {0xbc, 0x3a, 0x12, 0x01, 0xf0, 0xff};
        memcpy(b->data, c10, multipart_length(0));
        memcpy(b->data + multipart_offset(1), mono, sizeof(mono));
        memcpy(b->data + multipart_offset(2), c12, sizeof(c12));
        return;
    }
    if (point_cloud_mode()) {
        point_cloud_payload(b);
        return;
    }
    if (scalar_mode()) {
        scalar_payload(b);
        return;
    }
    const unsigned char opaque[]={0,0,0x80,0x3f,0,0,0,0x40,0,0,0x40,0x40};
    const unsigned char pixels[]={1,2,3,4,5,6,7,8,9,10,11,12};
    const unsigned char confidence[]={9,8,7,6,5,4};
    memcpy(b->data,opaque,sizeof(opaque));
    memcpy(b->data+24,pixels,multipart_length(1));
    memcpy(b->data+48,confidence,multipart_length(2));
}
#if MODE != 68
EXPORT int DSGetNumBufferParts(H h,H buffer,uint32_t *out){
    (void)h;(void)buffer;if(!multipart_mode())return -1003;trace("part_count");
    *out=MODE==58?33:MODE==70?0:multipart_count();return 0;
}
#endif
#if MODE != 69
EXPORT int DSGetBufferPartInfo(H h,H buffer,uint32_t index,int cmd,int *datatype,void *out,size_t *size){
    (void)h;if(!multipart_mode())return -1003;Buffer *b=(Buffer*)buffer;trace("part_info");
    if(index>=multipart_count())return -1017;
    if(MODE==71&&index==0&&cmd==0)sleep(30);
    if (job_part_mode()) {
        const uint64_t purpose[] = {1,65281,8,6,65282};
        unsigned identity = MODE == 198 ? index + 2 : index;
        switch (cmd) {
        case 0: INFO(10, uintptr_t, (uintptr_t)b->data + multipart_offset(index));
        case 1: INFO(12, size_t, multipart_length(index));
        case 2: INFO(12, size_t, identity < 2 ? 1 : identity == 2 ? 5 : identity == 3 ? 9 : 1000);
        case 3: INFO(8, uint64_t, identity < 2 ? (MODE == 195 && index == 1 && frame >= 2 ? 0x011000b8 : MODE == 193 ? 0x02180014 : 0x01080001) : identity == 2 ? 0x011000b8 : identity == 3 ? 0x010800c6 : 0x81080001);
        case 4: INFO(8, uint64_t, MODE == 194 && index == 1 ? 0 : MODE == 197 ? 1 : identity == 4 ? 1000 : 4);
        case 5: INFO(12, size_t, identity < 2 ? 2 : 1);
        case 6: case 11: INFO(12, size_t, identity == 1 ? 2 : 1);
        case 7: case 8: case 9: INFO(12, size_t, 0);
        case 10: INFO(8, uint64_t, 68);
        case 12: INFO(8, uint64_t, 7);
        case 13: INFO(8, uint64_t, purpose[identity]);
        default: return -1003;
        }
    }
    if (packed_range_mode()) {
        const uint64_t formats[] = {0x010a00d7, 0x01080001, 0x010c00da};
        switch (cmd) {
        case 2: INFO(12, size_t, index == 1 ? 1 : 5);
        case 3: INFO(8, uint64_t, formats[index]);
        case 4: INFO(8, uint64_t, MODE == 113 && index == 0 ? 1 : 4);
        case 5: INFO(12, size_t, index == 0 ? (MODE == 112 ? 3 : 4) : 2);
        case 6: case 11: INFO(12, size_t, 2);
        }
    }
    if (point_cloud_mode()) {
        const int confidence = index == 2 || (MODE == 82 && index == 1);
        switch (cmd) {
        case 2: INFO(12, size_t, index == 0 ? 5 : confidence ? 9 : 1);
        case 3: INFO(8, uint64_t, index == 0 ? (abc16_multipart_mode() ? 0x023000b9 : 0x026000c0) : confidence ? 0x011000c7 : 0x01100007);
        case 4: INFO(8, uint64_t, MODE == 119 && index == 0 ? 1 : 4);
        case 5: INFO(12, size_t, 3);
        case 6: case 11: INFO(12, size_t, 1);
        }
    }
    if (scalar_mode()) {
        const size_t types[3] = {5, MODE == 78 ? 0 : 1, 9};
        const uint64_t formats[3] = {0x011000b8, 0x01100007, 0x011000c7};
        switch (cmd) {
        case 2: INFO(12, size_t, types[index]);
        case 3: INFO(8, uint64_t, formats[index]);
        case 4: INFO(8, uint64_t, 4);
        case 5: INFO(12, size_t, 3);
        case 6: case 11: INFO(12, size_t, 2);
        }
    }
    switch(cmd){
    case 0:{uintptr_t pointer=(uintptr_t)b->data+multipart_offset(index);if(MODE==59&&index==1)pointer=(uintptr_t)b->data-1;if(MODE==60&&index==1)pointer=(uintptr_t)b->data+b->size+1;INFO(10,uintptr_t,pointer);}
    case 1:INFO(12,size_t,MODE==61&&index==1?SIZE_MAX:multipart_length(index));
    case 2:INFO(12,size_t,MODE==57?0:index==0?5:index==1||multipart_two_images()?1:9);
    case 3:INFO(8,uint64_t,index==1?(multipart_wide()?0x01100007:0x01080001):index==2&&multipart_two_images()?0x01080001:UINT64_C(0x123456789abcdef0));
    case 4:INFO(8,uint64_t,MODE==72&&index==1?3:index==1||(index==2&&multipart_two_images())?4:1000);
    case 5:if(MODE==64&&index==1){INFO(8,uint64_t,3);}INFO(12,size_t,index==0?1:3);
    case 6:case 11:INFO(12,size_t,index==0?1:2);
    case 7:case 8:INFO(12,size_t,0);
    case 9:INFO(12,size_t,(MODE==65||MODE==75)&&index==1?1:0);
    case 10:if(MODE==74&&index==1){uint64_t value=UINT64_MAX;return info(out,size,datatype,8,&value,7);}INFO(8,uint64_t,index==0?UINT64_MAX-1:UINT64_MAX);
    case 12:if(index==0)return -1014;INFO(8,uint64_t,UINT64_C(0x100000001));
    case 13:INFO(8,uint64_t,UINT64_C(0x200000001)+index);
    default:return -1003;
    }
}
#endif
/* EMVA GenDC 1.1/1.2 final linear descriptor. All integers are little endian. */
static void gdc_put(unsigned char *bytes, size_t offset, unsigned width, uint64_t value) {
    for (unsigned i = 0; i < width; i++) {
        bytes[offset + i] = (unsigned char)value;
        value >>= 8;
    }
}
static int gdc_padded(void) { return MODE >= 102 && MODE <= 104; }
static int gdc_second_is_2d(void) { return MODE == 96 || MODE == 98 || MODE == 102 || MODE == 103; }
static size_t gdc_intensity_length(void) { return gdc_padded() ? 11 : 6; }
static size_t gdc_range_length(void) {
    if (MODE == 101) return 8;
    if (MODE == 120) return 12;
    if (MODE == 102 || MODE == 103) return 12;
    if (MODE == 104) return 7;
    return MODE == 96 || MODE == 98 ? 4 : 24;
}
static size_t gdc_descriptor_size(void) { return gdc_second_is_2d() ? 424 : 432; }
static size_t gdc_size(void) {
    return gdc_descriptor_size() + gdc_intensity_length() + gdc_range_length() + 12 + (MODE == 87 ? 1 : 0);
}
static void gdc_component(unsigned char *header, uint64_t type, uint32_t format,
                          size_t part, uint64_t timestamp, unsigned group) {
    gdc_put(header, 0, 2, 0x2000);
    gdc_put(header, 4, 4, 56);
    gdc_put(header, 10, 2, group);
    gdc_put(header, 12, 2, 8);
    gdc_put(header, 14, 2, 9);
    gdc_put(header, 16, 4, 10);
    gdc_put(header, 20, 4, 11);
    gdc_put(header, 24, 8, timestamp);
    gdc_put(header, 32, 8, type);
    gdc_put(header, 40, 4, format);
    gdc_put(header, 46, 2, 1);
    gdc_put(header, 48, 8, part);
}
static void gdc_part(unsigned char *header, unsigned type, unsigned size,
                     uint32_t format, size_t offset, uint64_t length) {
    gdc_put(header, 0, 2, type);
    gdc_put(header, 4, 4, size);
    gdc_put(header, 8, 4, format);
    gdc_put(header, 16, 8, offset);
    gdc_put(header, 24, 8, length);
    gdc_put(header, 32, 8, offset);
}
static void gendc_payload(Buffer *buffer) {
    unsigned char *bytes = buffer->data;
    size_t descriptor = gdc_descriptor_size();
    size_t range_size = gdc_range_length();
    size_t range_start = descriptor + gdc_intensity_length();
    size_t metadata = range_start + range_size + (MODE == 87 ? 1 : 0);
    uint32_t range_format = MODE == 104 ? 0x011000c7 :
                            MODE == 101 ? 0x022000bb :
                            MODE == 120 ? 0x023000b9 :
                            MODE == 98 ? 0x01080001 :
                            gdc_second_is_2d() ? 0x011000b8 : 0x026000c0;
    const uint32_t words[] = {
        0x7fc01234, 0x80000000, 0x7f800000, 0xff800000, 0x00000001, 0x3f800000
    };
    memset(bytes, 0, descriptor);
    memcpy(bytes, "GNDC", 4);
    bytes[4] = MODE == 93 ? 2 : 1;
    bytes[5] = MODE == 85 ? 2 : 1;
    gdc_put(bytes, 8, 2, 0x1000);
    gdc_put(bytes, 10, 2, MODE == 85 ? 1 : MODE == 86 ? 2 : 0);
    gdc_put(bytes, 12, 4, 80);
    gdc_put(bytes, 16, 8, UINT64_C(0xf123456789abcdef));
    gdc_put(bytes, 24, 2, MODE == 88 ? 1 : 0);
    gdc_put(bytes, 32, 8, gdc_size() - descriptor);
    gdc_put(bytes, 40, 8, descriptor);
    gdc_put(bytes, 48, 4, descriptor);
    gdc_put(bytes, 52, 4, 3);
    gdc_put(bytes, 56, 8, 80);
    gdc_put(bytes, 64, 8, MODE == 91 ? 80 : 136);
    gdc_put(bytes, 72, 8, 192);
    gdc_component(bytes + 80, 1, 0x01080001, 248, UINT64_C(9007199254740993), 7);
    if (MODE == 86) gdc_put(bytes + 80, 2, 2, 1);
    gdc_component(bytes + 136, MODE == 98 ? 1 : MODE == 104 ? 6 : 4,
                  range_format, 304, UINT64_C(9007199254740995), 7);
    gdc_component(bytes + 192, 0x8001, 0x01080116, descriptor - 64, 456, 7);

    gdc_part(bytes + 248, 0x4200, 56, 0x01080001, descriptor, gdc_intensity_length());
    gdc_put(bytes + 248, 40, 4, 3);
    gdc_put(bytes + 248, 44, 4, 2);
    gdc_put(bytes + 248, 48, 2, gdc_padded() ? 2 : MODE == 94 ? 1 : 0);
    if (gdc_padded()) gdc_put(bytes + 248, 50, 2, 1);
    gdc_part(bytes + 304, gdc_second_is_2d() ? 0x4200 : 0x4100, gdc_second_is_2d() ? 56 : 64,
             range_format, MODE == 89 ? range_start - 1 : range_start,
             MODE == 90 ? UINT64_MAX : range_size);
    gdc_put(bytes + 304, 40, 4, 2);
    if (gdc_second_is_2d()) gdc_put(bytes + 304, 44, 4, gdc_padded() ? 2 : 1);
    if (MODE == 98) gdc_put(bytes + 304, 48, 2, 2);
    if (MODE == 102 || MODE == 103) {
        gdc_put(bytes + 304, 48, 2, 1);
        gdc_put(bytes + 304, 50, 2, MODE == 103 ? 3 : 2);
    }
    if (MODE == 104) gdc_put(bytes + 304, 48, 2, 3);
    gdc_part(bytes + descriptor - 64, 0x4000, 64, 0x01080116, metadata, 12);
    gdc_put(bytes + descriptor - 64, 40, 8, 12);
    gdc_put(bytes + descriptor - 64, 56, 8, UINT64_C(0x123456789abcdef0));

    if (gdc_padded()) {
        const unsigned char intensity[] = {1, 2, 3, 0xa1, 0xa2, 4, 5, 6, 0xb1, 0xb2, 0xc1};
        memcpy(bytes + descriptor, intensity, sizeof(intensity));
    } else {
        memcpy(bytes + descriptor, "\1\2\3\4\5\6", 6);
    }
    if (MODE == 102 || MODE == 103) {
        const unsigned char range[] = {
            0x34, 0x12, 0xdc, 0xfe, 0xd1, 0xff, 0xff, 0x01, 0x80, 0xd2, 0xe1, 0xe2
        };
        memcpy(bytes + range_start, range, sizeof(range));
    } else if (MODE == 104) {
        const unsigned char confidence[] = {0x34, 0x12, 0xff, 0xff, 0xf1, 0xf2, 0xf3};
        memcpy(bytes + range_start, confidence, sizeof(confidence));
    } else if (MODE == 120) {
        const uint16_t range[] = {0, 1, 0xffff, 0x8000, 0xfffe, 0x1234};
        for (unsigned i = 0; i < sizeof(range) / sizeof(range[0]); i++) {
            gdc_put(bytes, range_start + 2 * i, 2, range[i]);
        }
    } else if (MODE == 101) {
        const unsigned char range[] = {0x34, 0x12, 0xdc, 0xfe, 0xff, 0xff, 0x01, 0x80};
        memcpy(bytes + range_start, range, sizeof(range));
    } else if (MODE == 96 || MODE == 98) {
        gdc_put(bytes, range_start, 2, 0x1234);
        gdc_put(bytes, range_start + 2, 2, 0xffff);
    } else {
        for (unsigned i = 0; i < sizeof(words) / sizeof(words[0]); i++) {
            gdc_put(bytes, range_start + 4 * i, 4, words[i]);
        }
    }
    gdc_put(bytes, metadata, 4, 0xdecafbad);
    gdc_put(bytes, metadata + 4, 4, 0x1234);
    gdc_put(bytes, metadata + 8, 4, 4);
    trace("gendc_payload");
}
static size_t ids_row_bytes(void) { return IDS_BAYER_MODE ? (IDS_12 ? 12 : 10) : (IDS_12 ? 6 : 5); }
static size_t ids_height(void) { return IDS_BAYER_MODE ? 4 : 2; }
static size_t ids_image_end(void) { return IDS_CHUNKS ? 2 + (ids_row_bytes() + 2) * ids_height() + 1 : ids_row_bytes() * ids_height(); }
static size_t ids_filled(void) { return ids_image_end() + (IDS_CHUNKS ? 8 : 0) - (MODE == 133 ? 1 : 0); }
static void ids_payload(Buffer *buffer) {
    const unsigned char ten[10] = {0,85,170,255,249,255,170,85,0,6};
    const unsigned char twelve[12] = {0x12,0xab,0xc3,0x45,0xde,0xf6,0,0xff,0xf0,0x78,0x13,0x59};
    const unsigned char bayer_ten[40] = {0xe8,0x24,0xb5,0x78,0x39,0xc5,0x19,0xaa,0x55,0x8d,0x2d,0x8e,0x70,0xe3,0xc6,0x50,0xbb,0x1c,0xc1,0x36,0xb0,0x44,0xf7,0x16,0x87,0x7e,0xa9,0x21,0xd7,0x78,0x6a,0xd5,0x3b,0xa0,0x39,0xf1,0x4b,0x94,0x25,0x93};
    const unsigned char bayer_twelve[48] = {0xe7,0x2a,0x61,0xb4,0x58,0x3d,0xcf,0x19,0xb2,0xa0,0x46,0xf8,0x36,0x95,0xec,0x74,0xd1,0x92,0x5a,0x8f,0x07,0x21,0xbc,0x53,0xa9,0x45,0x75,0xf2,0x16,0xb0,0x8c,0x63,0x9d,0xd8,0x28,0xa6,0x68,0xc3,0xf1,0x1d,0xa2,0x64,0x47,0x9b,0x2e,0x35,0xe0,0x8a};
    const unsigned char *data = IDS_BAYER_MODE ? (IDS_12 ? bayer_twelve : bayer_ten) : (IDS_12 ? twelve : ten);
    size_t row = ids_row_bytes();
    memset(buffer->data, 0x91, ids_filled());
    if (IDS_CHUNKS) {
        for (size_t y = 0; y < ids_height(); y++)
            memcpy(buffer->data + 2 + y * (row + 2), data + y * row, row);
        memset(buffer->data + ids_image_end(), 0xff, 8);
    } else {
        memcpy(buffer->data, data, ids_filled());
    }
    trace("ids_grouped_payload");
}
/* Exact independently hand-derived PFNC literals, SHA256 56edd70880c9ec4d9b5a64bc98fc8e6b5f9662d699844f52e0af426ba522b10d.
 * Synthetic representation vectors, not physical/model-valid tiny ROIs. */
static const unsigned char packed_bicolor10[] = {0x01,0xfc,0x5f,0x95,0xaa,0x00,0x46,0xf0,0x3f,0x00,0xff,0x03,0x08,0x40,0x55,0x11,0xa8,0xba,0x6a,0x00};
static const unsigned char packed_bicolor12[] = {0x01,0xf0,0xff,0xbc,0x3a,0x12,0xff,0x0f,0x80,0x00,0x18,0x00,0x00,0xf0,0xff,0x55,0xa5,0xaa};
static const unsigned char packed_bicolor_odd10[] = {0x01,0xfc,0x5f,0x95,0xaa,0x00,0x46,0xf0,0x3f,0x80,0x00,0x54,0x15,0x81,0xaa};
static const unsigned char packed_bicolor_one12[] = {0xbc,0x3a,0x12};
static size_t packed_bicolor_row_bytes(void) { return PACKED_BICOLOR_10 ? 10 : MODE == 190 ? 3 : 9; }
static size_t packed_bicolor_image_end(void) {
    if (PACKED_BICOLOR_CHUNKS) return 2 + 2 * (packed_bicolor_row_bytes() + 2) + 1;
    if (MODE == 186) return sizeof(packed_bicolor_odd10);
    if (MODE == 187 || MODE == 190) return 3;
    return PACKED_BICOLOR_10 ? sizeof(packed_bicolor10) : sizeof(packed_bicolor12);
}
static size_t packed_bicolor_filled(void) { return packed_bicolor_image_end() + (PACKED_BICOLOR_CHUNKS ? 8 : 0); }
static int packed_bicolor_payload(Buffer *buffer) {
    size_t filled = packed_bicolor_filled();
    if (filled > buffer->size) { trace("packed_bicolor_payload_exceeds_announced_buffer"); return -1009; }
    if (PACKED_BICOLOR_CHUNKS) {
        const unsigned char *raw = PACKED_BICOLOR_10 ? packed_bicolor10 : packed_bicolor12;
        size_t row = packed_bicolor_row_bytes();
        buffer->data[0]=0xf1;buffer->data[1]=0xf2;
        for (size_t y=0;y<2;y++) { size_t at=2+y*(row+2);memcpy(buffer->data+at,raw+y*row,row);buffer->data[at+row]=0xde;buffer->data[at+row+1]=0xad; }
        buffer->data[packed_bicolor_image_end()-1]=0xbe;
        memset(buffer->data+packed_bicolor_image_end(),0xff,8);
    } else if (MODE == 186) memcpy(buffer->data,packed_bicolor_odd10,filled);
    else if (MODE == 190) memcpy(buffer->data,packed_bicolor_one12,filled);
    else memcpy(buffer->data,PACKED_BICOLOR_10 ? packed_bicolor10 : packed_bicolor12,filled);
    trace("packed_bicolor_payload");
    return 0;
}
static size_t bicolor_row_bytes(void) { return BICOLOR_BITS == 8 ? 6 : 12; }
static size_t bicolor_image_end(void) { return BICOLOR_CHUNKS ? 2 + (bicolor_row_bytes() + 2) * 2 + 1 : bicolor_row_bytes() * 2; }
static size_t bicolor_filled(void) { return bicolor_image_end() + (BICOLOR_CHUNKS ? 8 : 0); }
static int bicolor_payload(Buffer *buffer) {
    const uint16_t maximum = (UINT16_C(1) << BICOLOR_BITS) - 1;
    const uint16_t words[12] = {0, 1, maximum, maximum - 1, maximum / 2, 17, 33, 65, 97, 129, maximum - 7, maximum - 9};
    if (bicolor_filled() > buffer->size) { trace("bicolor_payload_exceeds_announced_buffer"); return -1009; }
    memset(buffer->data, 0x91, bicolor_filled());
    size_t bytes = BICOLOR_BITS == 8 ? 1 : 2;
    for (size_t y = 0; y < 2; y++) {
        size_t start = BICOLOR_CHUNKS ? 2 + y * (bicolor_row_bytes() + 2) : y * bicolor_row_bytes();
        for (size_t i = 0; i < 6; i++) {
            uint16_t value = words[y * 6 + i];
            if (bytes == 1) buffer->data[start + i] = (unsigned char)value;
            else if (BICOLOR_BIG) { buffer->data[start + 2 * i] = (unsigned char)(value >> 8); buffer->data[start + 2 * i + 1] = (unsigned char)value; }
            else { buffer->data[start + 2 * i] = (unsigned char)value; buffer->data[start + 2 * i + 1] = (unsigned char)(value >> 8); }
        }
    }
    if (BICOLOR_CHUNKS) memset(buffer->data + bicolor_image_end(), 0xff, 8);
    trace("bicolor_payload");
    return 0;
}
EXPORT int EventGetData(H h,void *out,size_t *size,uint64_t timeout) {(void)h;(void)timeout;if(!running||!acquiring||(lifecycle_mode()&&!lockreg)||(MODE==75&&!transferreg))return -1011;
#if MODE == 200
    const char *epoch_loss = getenv("CAPTUREFAB_GENTL_MOCK_EPOCH_LOSS");
    if (epoch_loss && !access(epoch_loss, F_OK)) {
        trace("industrial_epoch_lost");
        _exit(9);
    }
    /* Bounded synthetic100Hz source, not a physical timing qualification. */
    usleep(10000);
#endif
if(MODE==26&&waits++<50){trace("wait_timeout");usleep((useconds_t)(timeout*1000));return -1011;}if(MODE==27){trace("stuck_event");sleep(30);}if(*size<sizeof(H)*2){*size=sizeof(H)*2;return -1016;}Buffer *b=NULL;for(unsigned i=0;i<count;i++)if(buffers[i]&&buffers[i]->queued){b=buffers[i];break;}if(!b)return -1008;if(IDS_MODE&&ids_filled()>b->size){trace("ids_payload_exceeds_announced_buffer");return -1009;}b->queued=0;frame++;if(PACKED_BICOLOR_MODE){int status=packed_bicolor_payload(b);if(status)return status;}else if(BICOLOR_MODE){int status=bicolor_payload(b);if(status)return status;}else if(IDS_MODE){ids_payload(b);}else if(gendc_mode()){gendc_payload(b);}else if(job_part_mode()){int status=job_part_payload(b);if(status)return status;}else if(multipart_mode()){multipart_payload(b);}else if(chunk_mode()){chunk_payload(b);}else if(MODE==115){
    const uint16_t samples[6] = {0, 1, 0xffff, 0x8000, 0xfffe, 0x1234};
    for (unsigned i = 0; i < 6; i++) point_cloud_word(b->data + 2 * i, samples[i], 2);
}else if(MODE==100){
    const unsigned char profile[]={0x12,0x34,0xfe,0xdc,0xff,0xff,0x80,0x01};
    memcpy(b->data,profile,sizeof(profile));
}else if(MODE==1||MODE==14){unsigned char raw[]={0xaa,0x55,1,2,3,4,5,6,0xff,0xff,7,8,9,10,11,12,0xff,0xff};memcpy(b->data,raw,sizeof(raw));}else{unsigned char raw[]={1,2,3,4,5,6};memcpy(b->data,raw,sizeof(raw));}H result[2]={b,NULL};memcpy(out,result,sizeof(result));*size=sizeof(result);trace("frame");return 0;}
EXPORT int DSGetBufferInfo(H h,H handle,int cmd,int *datatype,void *out,size_t *size) {(void)h;Buffer *b=(Buffer*)handle;if(PACKED_BICOLOR_MODE){switch(cmd){
    case 9:INFO(12,size_t,packed_bicolor_filled() - (MODE == 188 ? 1 : 0));
    case 10:INFO(12,size_t,PACKED_BICOLOR_WIDTH);
    case 11:case 22:INFO(12,size_t,PACKED_BICOLOR_HEIGHT);
    case 14:INFO(12,size_t,PACKED_BICOLOR_CHUNKS?2:0);
    case 15:INFO(12,size_t,PACKED_BICOLOR_CHUNKS?1:0);
    case 18:INFO(12,size_t,PACKED_BICOLOR_CHUNKS?2:0);
    case 19:INFO(12,size_t,1);
    case 20:INFO(8,uint64_t,PACKED_BICOLOR_CODE);
    case 21:INFO(8,uint64_t,MODE==189?1000:4);
    case 24:if(!PACKED_BICOLOR_CHUNKS)return -1003;INFO(8,uint64_t,UINT64_C(0xaabbccdd00112233));
    case 26:INFO(5,int32_t,MODE==191?0:(MODE==181||MODE==183||MODE==185)?2:1);
    case 30:INFO(11,uint8_t,PACKED_BICOLOR_CHUNKS);
    default:break;
}}if(BICOLOR_MODE){switch(cmd){
    case 9:INFO(12,size_t,bicolor_filled() - (MODE == 176 ? 1 : 0));
    case 10:INFO(12,size_t,3);
    case 11:case 22:INFO(12,size_t,2);
    case 14:INFO(12,size_t,BICOLOR_CHUNKS?2:0);
    case 15:INFO(12,size_t,BICOLOR_CHUNKS?1:0);
    case 18:INFO(12,size_t,BICOLOR_CHUNKS?2:0);
    case 19:INFO(12,size_t,1);
    case 20:INFO(8,uint64_t,BICOLOR_CODE);
    case 21:INFO(8,uint64_t,MODE==177?1000:4);
    case 24:if(!BICOLOR_CHUNKS)return -1003;INFO(8,uint64_t,UINT64_C(0xaabbccdd00112233));
    case 26:INFO(5,int32_t,MODE==175?0:BICOLOR_BIG?2:1);
    case 30:INFO(11,uint8_t,BICOLOR_CHUNKS);
    default:break;
}}if(IDS_MODE){switch(cmd){
    case 9:INFO(12,size_t,ids_filled());
    case 10:INFO(12,size_t,(MODE==148||MODE==164)?7:MODE==132?3:IDS_BAYER_MODE?8:4);
    case 11:case 22:INFO(12,size_t,ids_height());
    case 14:INFO(12,size_t,IDS_CHUNKS?2:0);
    case 15:INFO(12,size_t,IDS_CHUNKS?1:0);
    case 18:INFO(12,size_t,IDS_CHUNKS?2:0);
    case 19:INFO(12,size_t,1);
    case 20:INFO(8,uint64_t,IDS_CODE);
    case 21:INFO(8,uint64_t,(MODE==134||MODE==168)?1001:(MODE==128||MODE==138||MODE==139||MODE==144||MODE==145||MODE==166||MODE==167)?1000:4);
    case 24:if(!IDS_CHUNKS)return -1003;INFO(8,uint64_t,UINT64_C(0xaabbccdd00112233));
    case 26:INFO(5,int32_t,(MODE==135||MODE==149||MODE==165)?2:1);
    case 30:INFO(11,uint8_t,IDS_CHUNKS);
    default:break;
}}if((multipart_mode()||gendc_mode())&&(cmd==10||cmd==11||cmd==14||cmd==15||cmd==17||cmd==18||cmd==20||cmd==21||cmd==22))return -1008;switch(cmd){case 0:INFO(10,uintptr_t,(uintptr_t)b->data);case 1:INFO(12,size_t,b->size);case 3:INFO(8,uint64_t,MODE==12?UINT64_MAX:98765);case 4:INFO(11,uint8_t,MODE!=18);case 7:INFO(11,uint8_t,MODE==7||(MODE==99&&frame<=2));case 17:INFO(11,uint8_t,MODE!=17);case 9:INFO(12,size_t,MODE==115?12:MODE==100?8:gendc_mode()?gdc_size():multipart_mode()?multipart_filled():MODE==5?b->size+1:chunk_mode()?32:(MODE==1||MODE==14)?18:6);case 10:if(MODE==4){INFO(8,uint64_t,3);}INFO(12,size_t,(MODE==100||MODE==115)?2:3);case 11:case 22:INFO(12,size_t,(MODE==100||MODE==115)?1:2);case 14:INFO(12,size_t,(MODE==1||MODE==14||chunk_mode())?2:0);case 15:INFO(12,size_t,chunk_mode()?2:0);case 16:INFO(8,uint64_t,frame);case 18:INFO(12,size_t,chunk_mode()?4:(MODE==1||MODE==14)?2:0);case 19:INFO(12,size_t,gendc_mode()?11:multipart_mode()?10:MODE==6?10:MODE==48?4:1);case 20:INFO(8,uint64_t,MODE==115?0x023000b9:MODE==100?0x022000bb:MODE==20?0x81080001:(MODE==1||MODE==14||chunk_mode())?0x01100007:0x01080001);case 21:INFO(8,uint64_t,MODE==15?3:4);case 24:if(!chunk_mode())return -1003;if(MODE==51){INFO(5,uint64_t,1);}INFO(8,uint64_t,UINT64_C(0xaabbccdd00112233));case 26:
    if (gendc_mode()) return -1008;
    if (job_part_mode()) { INFO(5, int32_t, 2); }
    if (MODE == 100 || MODE == 115) { INFO(5, int32_t, 2); }
    if (packed_range_mode()) { INFO(5, int32_t, MODE == 111 ? 0 : MODE == 114 ? 2 : 1); }
    if (point_cloud_mode()) { INFO(5, int32_t, (MODE == 83 || MODE == 118) ? 0 : (MODE == 81 || MODE == 117) ? 2 : 1); }
    if (scalar_mode()) { INFO(5, int32_t, MODE == 79 ? 0 : MODE == 77 ? 2 : 1); }
    INFO(5,int32_t,(MODE==66||MODE==73)?0:MODE==55?2:MODE==14?0:(MODE==1||chunk_mode())?2:1);case 27:if(!gendc_mode()||MODE==97)return -1008;INFO(12,size_t,gdc_size()+(MODE==92?1:0));case 28:if(MODE==12||MODE==21)return -1003;INFO(8,uint64_t,1234000);case 29:INFO(11,uint8_t,0);case 30:if(gendc_mode())return -1014;if(MODE==46||MODE==47)return -1003;INFO(11,uint8_t,(chunk_mode()&&MODE!=45)||MODE==67);case 31:INFO(11,uint8_t,MODE==19||MODE==95);default:return -1003;}}
